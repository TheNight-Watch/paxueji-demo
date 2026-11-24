//! 字形渲染：ab_glyph 加载思源黑体 + 按字符光栅化缓存（FIFO 上限 ~2000）。
//!
//! Python 版（ui.py `_load_font` + `_blit_text`）用 MaixPy `image.load_font`
//! 加载 SourceHanSansCN，draw_string 中文光栅化是大头（每帧 ~35ms），靠
//! 90 项「文本芯片」缓存兜底。Rust 侧光栅化一次后缓存 **单个字形位图**，
//! 之后整串 blit 都是亚毫秒级——芯片缓存不再需要，只保留字形缓存
//! （FIFO 淘汰，同 Python 芯片缓存的 `pop(0)` 语义）。
//!
//! 字体路径候选（照 ui.py 217-226）：
//! 1. `/maixapp/share/font/SourceHanSansCN-Regular.otf`（设备）
//! 2. `<exe_dir>/assets/SourceHanSansCN-Regular.otf`（随二进制部署兜底）
//! 3. 都失败：报 error 并回退内置 ASCII 位图字体（headless/PC 可用，
//!    中文渲染为 tofu 方框）
//!
//! 字号两档：20（正文）/26（标题）。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use ab_glyph::{Font as _, FontVec, Glyph, PxScale, ScaleFont as _};

use crate::image::{ClipRect, Color, GlyphBitmap, Image};

/// 内置 ASCII 位图字体的字号档（5x7 点阵 × scale）。
mod fallback;

/// 设备上的思源黑体路径（device-findings：`/maixapp/share/font/`）。
pub const DEVICE_FONT_PATH: &str = "/maixapp/share/font/SourceHanSansCN-Regular.otf";

/// 默认字形缓存上限（两档字号共用；CJK 常用字 + ASCII 远小于此）。
pub const GLYPH_CACHE_CAP: usize = 2000;

/// 标准字体候选路径（设备路径 + exe 同目录 assets 兜底）。
pub fn font_candidates() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from(DEVICE_FONT_PATH)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("assets").join("SourceHanSansCN-Regular.otf"));
        }
    }
    v
}

/// 字体集：一份字体数据 + 按 (字号, 字符) 缓存的字形位图。
pub struct FontSet {
    font: Option<FontVec>,
    /// 实际加载的字体路径（None = 内置 ASCII 回退）。
    pub source: Option<PathBuf>,
    /// 字形缓存（FIFO 淘汰，插入序）。
    cache: HashMap<(u16, char), Arc<GlyphBitmap>>,
    order: VecDeque<(u16, char)>,
    cap: usize,
}

impl FontSet {
    /// 依次尝试候选路径（含 TTC 集合的多个 face 索引）加载字体。
    pub fn load(paths: &[PathBuf]) -> Result<FontSet, String> {
        let mut last_err = String::from("无候选路径");
        for path in paths {
            if !path.is_file() {
                last_err = format!("{}: 不存在", path.display());
                continue;
            }
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(e) => {
                    last_err = format!("{}: 读取失败 {}", path.display(), e);
                    continue;
                }
            };
            // 常规字体 index 0 即可；TTC 集合前几个 face 逐个试
            for idx in 0..4u32 {
                match FontVec::try_from_vec_and_index(data.clone(), idx) {
                    Ok(font) => {
                        log::info!("字体已加载: {} (face {})", path.display(), idx);
                        return Ok(FontSet {
                            font: Some(font),
                            source: Some(path.clone()),
                            cache: HashMap::new(),
                            order: VecDeque::new(),
                            cap: GLYPH_CACHE_CAP,
                        });
                    }
                    Err(e) => {
                        last_err = format!("{}: {}", path.display(), e);
                        // 单字体文件继续试下一索引没有意义，只有 TTC 才多 face
                        if idx == 0 && !is_collection(&data) {
                            break;
                        }
                    }
                }
            }
        }
        Err(last_err)
    }

    /// 加载或回退内置 ASCII 位图字体（回退时 log::error，保证 headless 可用）。
    pub fn load_or_fallback(paths: &[PathBuf]) -> FontSet {
        match FontSet::load(paths) {
            Ok(f) => f,
            Err(e) => {
                log::error!("加载中文字体失败（{}），回退内置 ASCII 位图字体（中文将显示为方框）", e);
                FontSet::fallback()
            }
        }
    }

    /// 内置 ASCII 位图字体。
    pub fn fallback() -> FontSet {
        FontSet {
            font: None,
            source: None,
            cache: HashMap::new(),
            order: VecDeque::new(),
            cap: GLYPH_CACHE_CAP,
        }
    }

    /// 指定缓存上限（测试用）。
    pub fn with_cache_cap(mut self, cap: usize) -> Self {
        self.cap = cap;
        self
    }

    /// 是否为回退字体（测试「字体缺失跳过」判定用）。
    pub fn is_fallback(&self) -> bool {
        self.font.is_none()
    }

    /// 基线相对文本顶部的距离（行内排版用）。
    pub fn ascent(&self, px: u16) -> f32 {
        match &self.font {
            Some(font) => font.as_scaled(em_scale(font, px as f32)).ascent(),
            None => fallback::ascent(px),
        }
    }

    /// 取（或光栅化并缓存）一个字形。
    pub fn glyph(&mut self, px: u16, ch: char) -> Arc<GlyphBitmap> {
        let key = (px, ch);
        if let Some(g) = self.cache.get(&key) {
            return Arc::clone(g);
        }
        let bitmap = match &self.font {
            Some(font) => rasterize(font, px, ch),
            None => fallback::glyph(px, ch),
        };
        let arc = Arc::new(bitmap);
        self.cache.insert(key, Arc::clone(&arc));
        self.order.push_back(key);
        while self.cache.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            } else {
                break;
            }
        }
        arc
    }

    /// 画一串文本（(x, y) 为文本框左上角；clip 为芯片/掩膜矩形，出界像素丢弃，
    /// 对应 Python 版文本芯片的尺寸约束）。
    ///
    /// 参数即 Python `_blit_text` 的原料，逐项对应（不再收敛进结构体）。
    #[allow(clippy::too_many_arguments)]
    pub fn draw_string(
        &mut self,
        canvas: &mut Image,
        x: i32,
        y: i32,
        text: &str,
        fg: Color,
        px: u16,
        clip: Option<ClipRect>,
    ) {
        let baseline = y as f32 + self.ascent(px);
        let mut pen = x as f32;
        for ch in text.chars() {
            let g = self.glyph(px, ch);
            let gx = (pen + g.left as f32).round() as i32;
            let gy = (baseline + g.top as f32).round() as i32;
            canvas.draw_glyph_opts(gx, gy, &g, fg, clip);
            pen += g.advance;
        }
    }

    /// 缓存规模（测试/诊断）。
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }
}

fn is_collection(data: &[u8]) -> bool {
    // 'ttcf' magic（TrueType Collection）
    data.len() >= 4 && &data[0..4] == b"ttcf"
}

/// ab_glyph 的 `as_scaled(px)` 以 hhea 行高（ascent - descent）为「1 倍字号」，
/// 而本 crate 的 px 语义照 Python `draw_string`：**em 大小**（20px 的中文
/// 就是 ~20px 宽）。思源黑体 hhea 行高 1.16+0.288 em、微软雅黑 1.06+0.26 em，
/// 直接 as_scaled(px) 会让字比 Python 版小 ~30%。这里把请求换算回 em 口径：
/// `scale = px * 行高 / upem`，advance/边界/ascent 就都以 em 计。
fn em_scale(font: &FontVec, px: f32) -> f32 {
    let height = font.height_unscaled();
    let upem = font.units_per_em().unwrap_or(height).max(1.0);
    px * height / upem
}

/// 光栅化一个字形（pen 位 (0,0)、基线 y=0，位图原点 = px_bounds 左上角）。
fn rasterize(font: &FontVec, px: u16, ch: char) -> GlyphBitmap {
    let scale = em_scale(font, px as f32);
    let scaled = font.as_scaled(scale);
    let gid = font.glyph_id(ch);
    let advance = scaled.h_advance(gid);
    let glyph = Glyph {
        id: gid,
        scale: PxScale::from(scale),
        position: ab_glyph::Point { x: 0.0, y: 0.0 },
    };
    match font.outline_glyph(glyph) {
        Some(outlined) => {
            let bb = outlined.px_bounds();
            let left = bb.min.x.round() as i32;
            let top = bb.min.y.round() as i32;
            let w = (bb.max.x.ceil() - bb.min.x.floor()).max(0.0) as usize;
            let h = (bb.max.y.ceil() - bb.min.y.floor()).max(0.0) as usize;
            let mut data = vec![0u8; w * h];
            if w > 0 && h > 0 {
                outlined.draw(|x, y, coverage| {
                    let v = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
                    // draw() 的 x/y 以 px_bounds 左上角为原点
                    if let Some(slot) = data.get_mut(y as usize * w + x as usize) {
                        *slot = v;
                    }
                });
            }
            GlyphBitmap {
                width: w as i32,
                height: h as i32,
                data,
                left,
                top,
                advance,
            }
        }
        None => GlyphBitmap {
            width: 0,
            height: 0,
            data: Vec::new(),
            left: 0,
            top: 0,
            advance,
        },
    }
}

/// 单测共用的 Windows 字体候选（msyh.ttc / simhei.ttf 若存在）。
#[cfg(test)]
pub(crate) fn windows_font_candidates() -> Vec<PathBuf> {
    [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ]
    .iter()
    .map(PathBuf::from)
    .filter(|p| p.is_file())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn glyph_cache_fifo_cap() {
        let mut fs = FontSet::fallback().with_cache_cap(8);
        for i in 0..20u8 {
            fs.glyph(20, char::from(b'a' + (i % 20)));
        }
        assert!(fs.cache_len() <= 8);
        // 插入第 9 个后最老的 'a' 被淘汰
        let mut fs2 = FontSet::fallback().with_cache_cap(8);
        let chars: Vec<char> = "abcdefgh".chars().collect();
        for &c in &chars {
            fs2.glyph(20, c);
        }
        assert_eq!(fs2.cache_len(), 8);
        fs2.glyph(20, 'z');
        assert_eq!(fs2.cache_len(), 8);
        // 'a'（最老）已不在缓存，其余 7 个 + 'z' 在
        let g = fs2.glyph(20, 'a');
        assert_eq!(fs2.cache_len(), 8); // 重新插入
        assert!(g.width > 0);
    }

    #[test]
    fn fallback_font_renders_ascii() {
        let mut fs = FontSet::fallback();
        assert!(fs.is_fallback());
        let mut im = Image::new(200, 40);
        fs.draw_string(&mut im, 0, 0, "Hello", Color(255, 255, 255), 20, None);
        // 有墨迹
        let ink = im.data.iter().filter(|&&b| b > 0).count();
        assert!(ink > 50, "fallback 字体应有大量笔画像素，实际 {}", ink);
        // 高度 7*scale=14 以内
        let mut max_y = 0;
        for y in 0..40 {
            for x in 0..200 {
                if im.pixel(x, y).map(|c| c.0 > 0) == Some(true) {
                    max_y = max_y.max(y);
                }
            }
        }
        assert!(max_y < 16, "20px 回退字形应 ≤14px 高，实际 {}", max_y + 1);
    }

    #[test]
    fn fallback_tofu_for_cjk() {
        let mut fs = FontSet::fallback();
        let g = fs.glyph(20, '中');
        assert!(g.width > 0 && g.height > 0, "回退字体 CJK 应给 tofu 方框");
    }

    #[test]
    fn real_font_rasterizes_and_caches() {
        let cands = windows_font_candidates();
        if cands.is_empty() {
            eprintln!("跳过：本机无 msyh/simhei 字体");
            return;
        }
        let mut fs = FontSet::load(&cands).expect("Windows 字体应能加载");
        assert!(!fs.is_fallback());
        let mut im = Image::new(200, 50);
        fs.draw_string(&mut im, 0, 0, "O5 语音助手", Color(255, 255, 255), 26, None);
        let ink = im.data.iter().filter(|&&b| b > 0).count();
        assert!(ink > 100, "CJK 文本应有大量笔画像素");
        // 缓存生效：重复绘制不增长
        let n = fs.cache_len();
        fs.draw_string(&mut im, 0, 0, "O5 语音助手", Color(255, 255, 255), 26, None);
        assert_eq!(fs.cache_len(), n);
        // 两档字号独立缓存
        fs.glyph(20, '中');
        assert_eq!(fs.cache_len(), n + 1);
    }

    #[test]
    fn real_font_advances_match_units_estimate() {
        let cands = windows_font_candidates();
        if cands.is_empty() {
            eprintln!("跳过：本机无 msyh/simhei 字体");
            return;
        }
        let mut fs = FontSet::load(&cands).unwrap();
        // CJK 字形在 px=20 时的 advance 应接近 20（满宽 em 口径），est 语义成立
        let g = fs.glyph(20, '中');
        assert!((g.advance - 20.0).abs() < 2.0, "CJK advance={} 应≈20", g.advance);
    }

    #[test]
    fn draw_string_clip_drops_outside() {
        let mut fs = FontSet::fallback();
        let mut a = Image::new(200, 40);
        let mut b = Image::new(200, 40);
        let clip = ClipRect::new(0, 0, 40, 40);
        fs.draw_string(&mut a, 0, 0, "Hello World", Color(255, 255, 255), 20, None);
        fs.draw_string(&mut b, 0, 0, "Hello World", Color(255, 255, 255), 20, Some(clip));
        let ink_a: usize = a.data.iter().map(|&v| v as usize).sum();
        let ink_b: usize = b.data.iter().map(|&v| v as usize).sum();
        assert!(ink_b < ink_a, "裁剪应丢弃 clip 外的笔画");
        assert!(ink_b > 0);
    }

    #[test]
    fn load_missing_path_falls_back() {
        let fs = FontSet::load_or_fallback(&[Path::new("Z:/no/such/font.otf").to_path_buf()]);
        assert!(fs.is_fallback());
        assert!(fs.source.is_none());
    }
}

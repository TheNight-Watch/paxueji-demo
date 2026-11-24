//! RGB888 画布与软件绘制原语。
//!
//! 对应 MaixPy `image.Image`（FMT_RGB888）在本项目里用到的最小子集：
//! `draw_rect`（thickness=-1 实心 / >=1 描边）、`draw_image`（blit）、
//! 以及 Python `ui.py` 里手写的 `_blend_strip` / 掩膜字形写入。
//! 全部原语软件渲染、纯 CPU、无 unsafe，坐标越界一律裁剪（负坐标合法）。

/// RGB888 颜色（Python 侧是 `(r, g, b)` 元组）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    /// 每通道 +v（封顶 255）——Python 按钮按下高亮 `min(255, c + 40)` 的配方。
    pub fn brighten(self, v: u8) -> Color {
        Color(
            self.0.saturating_add(v),
            self.1.saturating_add(v),
            self.2.saturating_add(v),
        )
    }
}

/// 字形位图：8bit 灰度覆盖度（255 = 实心笔画）+ 相对笔位（基线）的摆放偏移。
/// 覆盖度即 Python 版 GRAYSCALE 掩膜的灰度值，`draw_glyph` 按它做透明写。
#[derive(Clone, Debug)]
pub struct GlyphBitmap {
    pub width: i32,
    pub height: i32,
    /// 灰度覆盖度，行优先，len = width * height。
    pub data: Vec<u8>,
    /// 位图左边缘相对笔位 x 的偏移（bearing x）。
    pub left: i32,
    /// 位图顶边缘相对基线的偏移（通常为负）。
    pub top: i32,
    /// 光栅化后前进量（px，浮点字距）。
    pub advance: f32,
}

/// 像素裁剪矩形（左上角 + 宽高，右/下边界开区间）。
/// 对应 Python 版文本芯片（`_blit_text` 的 `w`/`hh`）与掩膜的尺寸约束。
#[derive(Clone, Copy, Debug)]
pub struct ClipRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl ClipRect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> ClipRect {
        ClipRect { x, y, w, h }
    }
}

/// RGB888 图像（`data.len() == width * height * 3`，行优先）。
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Image {
    /// 新画布（全黑零初始化；主循环里每帧都会整屏覆盖）。
    pub fn new(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            data: vec![0u8; (width as usize) * (height as usize) * 3],
        }
    }

    /// 从现有 RGB888 字节构造（长度不符返回 None）。
    pub fn from_rgb(width: u32, height: u32, data: Vec<u8>) -> Option<Image> {
        if data.len() != (width as usize) * (height as usize) * 3 {
            return None;
        }
        Some(Image {
            width,
            height,
            data,
        })
    }

    #[inline]
    fn stride(&self) -> usize {
        self.width as usize * 3
    }

    #[inline]
    fn clip_x(&self, x: i32) -> Option<usize> {
        if x < 0 || x >= self.width as i32 {
            None
        } else {
            Some(x as usize)
        }
    }

    #[inline]
    fn clip_y(&self, y: i32) -> Option<usize> {
        if y < 0 || y >= self.height as i32 {
            None
        } else {
            Some(y as usize)
        }
    }

    /// 读一个像素（越界返回 None）。
    pub fn pixel(&self, x: i32, y: i32) -> Option<Color> {
        let (x, y) = (self.clip_x(x)?, self.clip_y(y)?);
        let i = y * self.stride() + x * 3;
        Some(Color(
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
        ))
    }

    /// 写一个像素（越界忽略——所有原语的裁剪出口）。
    pub fn set_pixel(&mut self, x: i32, y: i32, c: Color) {
        if let (Some(x), Some(y)) = (self.clip_x(x), self.clip_y(y)) {
            let i = y * self.stride() + x * 3;
            self.data[i] = c.0;
            self.data[i + 1] = c.1;
            self.data[i + 2] = c.2;
        }
    }

    /// 整屏填色。
    pub fn fill(&mut self, c: Color) {
        for px in self.data.chunks_exact_mut(3) {
            px[0] = c.0;
            px[1] = c.1;
            px[2] = c.2;
        }
    }

    /// 实心矩形（MaixPy `draw_rect(thickness=-1)` 语义）。
    /// x/y 可为负，出画布部分裁剪。
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i32);
        let y1 = (y + h).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let stride = self.stride();
        let fill_row = |row: &mut [u8]| {
            for px in row.chunks_exact_mut(3) {
                px[0] = c.0;
                px[1] = c.1;
                px[2] = c.2;
            }
        };
        for yy in y0..y1 {
            let base = yy as usize * stride + x0 as usize * 3;
            fill_row(&mut self.data[base..base + (x1 - x0) as usize * 3]);
        }
    }

    /// 描边矩形（MaixPy `draw_rect(thickness>=1)` 语义：边框画在矩形边界上，
    /// thickness>1 向内加粗；thickness>=min(w,h)/2 时等效实心）。
    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color, thickness: i32) {
        if thickness < 1 {
            self.fill_rect(x, y, w, h, c);
            return;
        }
        if w <= 0 || h <= 0 {
            return;
        }
        let t = thickness.min(w).min(h);
        self.fill_rect(x, y, w, t, c); // 上
        self.fill_rect(x, y + h - t, w, t, c); // 下
        self.fill_rect(x, y + t, t, (h - 2 * t).max(0), c); // 左
        self.fill_rect(x + w - t, y + t, t, (h - 2 * t).max(0), c); // 右
    }

    /// 位图 blit（MaixPy `draw_image`）：把 `src` 画到 (x, y)，出画布部分裁剪。
    pub fn draw_image(&mut self, x: i32, y: i32, src: &Image) {
        if src.width == 0 || src.height == 0 {
            return;
        }
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + src.width as i32).min(self.width as i32);
        let y1 = (y + src.height as i32).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let dst_stride = self.stride();
        let src_stride = src.stride();
        for yy in y0..y1 {
            let sy = (yy - y) as usize * src_stride;
            let dy = yy as usize * dst_stride;
            let sx = (x0 - x) as usize * 3 + sy;
            let dx = x0 as usize * 3 + dy;
            self.data[dx..dx + (x1 - x0) as usize * 3]
                .copy_from_slice(&src.data[sx..sx + (x1 - x0) as usize * 3]);
        }
    }

    /// 半透明压暗横带（Python `ui.py _blend_strip` 的 alpha=110 默认配方）：
    /// `out = 画布 * (110/256) + color * (146/256)`——画布权重约 43%，
    /// 透出（被压暗后的）摄像头画面。逐通道四舍五入。
    pub fn blend_strip(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        self.blend_strip_alpha(x, y, w, h, color, 110);
    }

    /// `blend_strip` 的通用 alpha 版（alpha = 画布权重 / 256）。
    pub fn blend_strip_alpha(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color, alpha: u16) {
        if w <= 0 || h <= 0 {
            return;
        }
        let alpha = alpha.min(256) as u32;
        let beta = 256 - alpha;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i32);
        let y1 = (y + h).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let stride = self.stride();
        for yy in y0..y1 {
            let base = yy as usize * stride + x0 as usize * 3;
            let row = &mut self.data[base..base + (x1 - x0) as usize * 3];
            for px in row.chunks_exact_mut(3) {
                for (k, fc) in [color.0, color.1, color.2].into_iter().enumerate() {
                    let v = (px[k] as u32 * alpha + fc as u32 * beta + 128) >> 8;
                    px[k] = v.min(255) as u8;
                }
            }
        }
    }

    /// 字形透明写（Python `ui.py` 掩膜 blend 配方：`region.blend(纯色, alpha=0,
    /// mask=灰度掩膜)`）：覆盖度 255 处写成实色 fg（alpha=0 = 全取前景），
    /// 覆盖度 0 处保持画布原样，中间值按覆盖度插值——文字保持实色可读、
    /// 底透出（被字幕带压暗后的）摄像头画面。
    pub fn draw_glyph(&mut self, x: i32, y: i32, glyph: &GlyphBitmap, fg: Color) {
        self.draw_glyph_opts(x, y, glyph, fg, None);
    }

    /// [`Image::draw_glyph`] 的带裁剪版（文本芯片矩形约束）。
    pub fn draw_glyph_opts(
        &mut self,
        x: i32,
        y: i32,
        glyph: &GlyphBitmap,
        fg: Color,
        clip: Option<ClipRect>,
    ) {
        if glyph.width <= 0 || glyph.height <= 0 {
            return;
        }
        let x0 = match &clip {
            Some(c) => x.max(c.x),
            None => x,
        }
        .max(0);
        let y0 = match &clip {
            Some(c) => y.max(c.y),
            None => y,
        }
        .max(0);
        let x1 = (x + glyph.width)
            .min(match &clip {
                Some(c) => c.x + c.w,
                None => i32::MAX,
            })
            .min(self.width as i32);
        let y1 = (y + glyph.height)
            .min(match &clip {
                Some(c) => c.y + c.h,
                None => i32::MAX,
            })
            .min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let stride = self.stride();
        let gw = glyph.width as usize;
        for yy in y0..y1 {
            let gy = (yy - y) as usize;
            let base = yy as usize * stride + x0 as usize * 3;
            for xx in x0..x1 {
                let gx = (xx - x) as usize;
                let cov = glyph.data[gy * gw + gx] as u32;
                if cov == 0 {
                    continue;
                }
                let i = base + (xx - x0) as usize * 3;
                if cov == 255 {
                    self.data[i] = fg.0;
                    self.data[i + 1] = fg.1;
                    self.data[i + 2] = fg.2;
                } else {
                    let inv = 255 - cov;
                    for (k, fc) in [fg.0, fg.1, fg.2].into_iter().enumerate() {
                        let v = (self.data[i + k] as u32 * inv + fc as u32 * cov + 127) / 255;
                        self.data[i + k] = v.min(255) as u8;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> Image {
        Image::new(16, 10)
    }

    #[test]
    fn fill_rect_basics_and_clip() {
        let mut im = img();
        im.fill_rect(2, 3, 4, 2, Color(1, 2, 3));
        assert_eq!(im.pixel(2, 3), Some(Color(1, 2, 3)));
        assert_eq!(im.pixel(5, 4), Some(Color(1, 2, 3)));
        // 边界半开：x=6 / y=5 不在矩形内
        assert_eq!(im.pixel(6, 3), Some(Color(0, 0, 0)));
        assert_eq!(im.pixel(2, 5), Some(Color(0, 0, 0)));

        // 负坐标 + 出界裁剪（覆盖 [0,4)x[0,4)）
        im.fill_rect(-4, -4, 8, 8, Color(9, 9, 9));
        assert_eq!(im.pixel(0, 0), Some(Color(9, 9, 9)));
        assert_eq!(im.pixel(3, 3), Some(Color(9, 9, 9)));
        // (0,4) 在两个矩形之外（(4,4) 已被第一个矩形覆盖）
        assert_eq!(im.pixel(0, 4), Some(Color(0, 0, 0)));

        // 宽/高为 0 或负 → no-op
        im.fill_rect(0, 0, 0, 5, Color(255, 255, 255));
        assert_eq!(im.pixel(0, 0), Some(Color(9, 9, 9)));

        // 整屏
        im.fill_rect(0, 0, 16, 10, Color(7, 8, 9));
        assert_eq!(im.pixel(15, 9), Some(Color(7, 8, 9)));
    }

    #[test]
    fn draw_rect_border() {
        let mut im = img();
        im.draw_rect(2, 2, 6, 4, Color(5, 6, 7), 1);
        // 边框 1px
        assert_eq!(im.pixel(2, 2), Some(Color(5, 6, 7)));
        assert_eq!(im.pixel(7, 5), Some(Color(5, 6, 7)));
        assert_eq!(im.pixel(2, 5), Some(Color(5, 6, 7)));
        assert_eq!(im.pixel(7, 2), Some(Color(5, 6, 7)));
        // 内部为空
        assert_eq!(im.pixel(4, 4), Some(Color(0, 0, 0)));

        // thickness=2：上下各 2 行
        let mut im2 = img();
        im2.draw_rect(0, 0, 6, 6, Color(1, 1, 1), 2);
        assert_eq!(im2.pixel(0, 1), Some(Color(1, 1, 1)));
        assert_eq!(im2.pixel(2, 1), Some(Color(1, 1, 1)));
        assert_eq!(im2.pixel(2, 2), Some(Color(0, 0, 0))); // 中心空洞
    }

    #[test]
    fn draw_image_blit_and_clip() {
        let src = Image::from_rgb(4, 4, vec![10u8; 48]).unwrap();
        let mut im = img();
        im.draw_image(2, 2, &src);
        assert_eq!(im.pixel(2, 2), Some(Color(10, 10, 10)));
        assert_eq!(im.pixel(5, 5), Some(Color(10, 10, 10)));
        assert_eq!(im.pixel(6, 6), Some(Color(0, 0, 0))); // src 只到 (5,5)

        // 部分出界：右下角裁剪，不 panic
        let mut im2 = img();
        im2.draw_image(14, 8, &src);
        assert_eq!(im2.pixel(14, 8), Some(Color(10, 10, 10)));
        assert_eq!(im2.pixel(15, 9), Some(Color(10, 10, 10)));
        assert_eq!(im2.pixel(15, 8), Some(Color(10, 10, 10)));
        assert_eq!(im2.pixel(0, 0), Some(Color(0, 0, 0)));
    }

    #[test]
    fn blend_strip_pixel_values() {
        // 白底压暗：out = (255*110 + 12*146 + 128) >> 8
        let mut im = img();
        im.fill(Color(255, 255, 255));
        im.blend_strip(0, 0, 16, 10, Color(12, 12, 16));
        let r = (255u32 * 110 + 12 * 146 + 128) >> 8;
        let b = (255u32 * 110 + 16 * 146 + 128) >> 8;
        assert_eq!(im.pixel(3, 3), Some(Color(r as u8, r as u8, b as u8)));

        // 精确断言（公式手算）：
        // R = (28050 + 1752 + 128) / 256 = 29930 / 256 = 116
        // B = (28050 + 2336 + 128) / 256 = 30514 / 256 = 119
        assert_eq!(im.pixel(0, 0), Some(Color(116, 116, 119)));

        // 同色混合不变
        let mut im2 = img();
        im2.fill(Color(30, 40, 50));
        im2.blend_strip(0, 0, 16, 10, Color(30, 40, 50));
        assert_eq!(im2.pixel(5, 5), Some(Color(30, 40, 50)));

        // alpha=0：完全取覆盖色（掩膜 blend 的 alpha=0 端点）
        let mut im3 = img();
        im3.fill(Color(200, 100, 50));
        im3.blend_strip_alpha(0, 0, 16, 10, Color(10, 20, 30), 0);
        assert_eq!(im3.pixel(7, 7), Some(Color(10, 20, 30)));

        // alpha=256：完全保留画布
        let mut im4 = img();
        im4.fill(Color(200, 100, 50));
        im4.blend_strip_alpha(0, 0, 16, 10, Color(10, 20, 30), 256);
        assert_eq!(im4.pixel(7, 7), Some(Color(200, 100, 50)));

        // 裁剪：出界部分不 panic，界内正常
        let mut im5 = img();
        im5.fill(Color(255, 255, 255));
        im5.blend_strip(14, 8, 10, 10, Color(12, 12, 16));
        assert_eq!(im5.pixel(15, 9), Some(Color(116, 116, 119)));
    }

    #[test]
    fn draw_glyph_coverage_write() {
        let g = GlyphBitmap {
            width: 2,
            height: 1,
            data: vec![255, 128],
            left: 0,
            top: 0,
            advance: 2.0,
        };
        let mut im = img();
        im.fill(Color(100, 100, 100));
        im.draw_glyph(1, 2, &g, Color(200, 0, 0));
        // 覆盖度 255 → 实色
        assert_eq!(im.pixel(1, 2), Some(Color(200, 0, 0)));
        // 覆盖度 128：每通道独立按 fg 混合
        //   R = (100*127 + 200*128 + 127)/255 = 150
        //   G/B = (100*127 + 0*128 + 127)/255 = 50
        let v = (100u32 * 127 + 200 * 128 + 127) / 255;
        let g0 = (100u32 * 127 + 127) / 255;
        assert_eq!(im.pixel(2, 2), Some(Color(v as u8, g0 as u8, g0 as u8)));
        // 邻近不受影响
        assert_eq!(im.pixel(3, 2), Some(Color(100, 100, 100)));
    }

    #[test]
    fn from_rgb_validates_len() {
        assert!(Image::from_rgb(2, 2, vec![0; 12]).is_some());
        assert!(Image::from_rgb(2, 2, vec![0; 11]).is_none());
    }
}

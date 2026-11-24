//! 预览缩放：手写双线性（纯 f32），不引图像库。
//!
//! Python 版用 MaixPy `img.resize()`；Rust 侧没有现成的零依赖 RGB888 缩放，
//! 按最通用的"像素中心对齐 + 双线性 + 边缘钳制"实现（放大缩小同一套系数）。
//! 预览是给人看的屏显副本，不做抗锯齿锯齿补偿之类的花活。

/// RGB888 双线性缩放：`(sw, sh)` 的 `rgb` 缩到 `(dw, dh)`。
///
/// 映射：目的像素中心 `(dx+0.5, dy+0.5)` 按比例反推回源坐标再减半个像素，
/// 越界钳制到源边缘；四邻按 f32 权重混合后四舍五入回 u8。
/// 尺寸相同直接拷贝；`dw`/`dh` 为 0 返回空。
pub fn bilinear_resize(rgb: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if dw == 0 || dh == 0 || sw == 0 || sh == 0 {
        return Vec::new();
    }
    if sw == dw && sh == dh {
        return rgb.to_vec();
    }
    debug_assert_eq!(rgb.len(), sw as usize * sh as usize * 3);

    let mut out = vec![0u8; dw as usize * dh as usize * 3];
    let x_ratio = sw as f32 / dw as f32;
    let y_ratio = sh as f32 / dh as f32;
    let sw_max = (sw - 1) as f32;
    let sh_max = (sh - 1) as f32;

    for dy in 0..dh {
        // 目的行 -> 源坐标（像素中心对齐）
        let sy = ((dy as f32 + 0.5) * y_ratio - 0.5).clamp(0.0, sh_max);
        let y0 = sy.floor() as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = sy - y0 as f32;
        let row0 = y0 as usize * sw as usize * 3;
        let row1 = y1 as usize * sw as usize * 3;
        for dx in 0..dw {
            let sx = ((dx as f32 + 0.5) * x_ratio - 0.5).clamp(0.0, sw_max);
            let x0 = sx.floor() as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = sx - x0 as f32;
            let c0 = x0 as usize * 3;
            let c1 = x1 as usize * 3;
            for ch in 0..3 {
                let p00 = rgb[row0 + c0 + ch] as f32;
                let p01 = rgb[row1 + c0 + ch] as f32;
                let p10 = rgb[row0 + c1 + ch] as f32;
                let p11 = rgb[row1 + c1 + ch] as f32;
                let top = p00 + (p10 - p00) * fx;
                let bot = p01 + (p11 - p01) * fx;
                let v = top + (bot - top) * fy;
                // 钳制 + 四舍五入（0.5 偏移后截断 = round-half-up）
                out[(dy as usize * dw as usize + dx as usize) * 3 + ch] =
                    (v.clamp(0.0, 255.0) + 0.5) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单通道值铺成 RGB（三个通道同值），便于手算期望矩阵。
    fn gray_frame(vals: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(vals.len() * 3);
        for &g in vals {
            v.extend_from_slice(&[g, g, g]);
        }
        assert_eq!(v.len(), w as usize * h as usize * 3);
        v
    }

    fn channel(frame: &[u8], ch: usize) -> Vec<u8> {
        frame[ch..].iter().step_by(3).copied().collect()
    }

    #[test]
    fn downscale_4x4_to_2x2_known_matrix() {
        // 4x4 源（行优先）：
        //  10  20  30  40
        //  50  60  70  80
        //  90 100 110 120
        // 130 140 150 160
        // 2x2 的每个目的像素中心恰好落在源 2x2 块的公共角点上（权重各 1/4），
        // 期望 = 对应 2x2 块均值：35 55 / 115 135
        let src = gray_frame(
            &[
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
            ],
            4,
            4,
        );
        let out = bilinear_resize(&src, 4, 4, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 3);
        assert_eq!(channel(&out, 0), vec![35, 55, 115, 135]);
        assert_eq!(channel(&out, 1), vec![35, 55, 115, 135]);
        assert_eq!(channel(&out, 2), vec![35, 55, 115, 135]);
    }

    #[test]
    fn upscale_2x2_to_3x3_known_matrix_with_clamp() {
        // 源（s[y][x]）：
        //   0 100
        // 200 255
        // 3x3 时边缘越界钳制（sx/sy 负数贴 0、超界贴 1），期望矩阵手算：
        //    0  50 100
        //  100 139 178
        //  200 228 255
        let src = gray_frame(&[0, 100, 200, 255], 2, 2);
        let out = bilinear_resize(&src, 2, 2, 3, 3);
        assert_eq!(
            channel(&out, 0),
            vec![0, 50, 100, 100, 139, 178, 200, 228, 255]
        );
    }

    #[test]
    fn same_size_is_copy_and_identity_content() {
        let src = gray_frame(&[1, 2, 3, 4], 2, 2);
        let out = bilinear_resize(&src, 2, 2, 2, 2);
        assert_eq!(out, src);
    }

    #[test]
    fn zero_dim_returns_empty() {
        assert!(bilinear_resize(&[1, 2, 3], 1, 1, 0, 5).is_empty());
        assert!(bilinear_resize(&[1, 2, 3], 1, 1, 5, 0).is_empty());
    }

    #[test]
    fn channels_stay_independent() {
        // R 通道竖条纹、B 通道横条纹、G 常量：缩放后各通道方向性保持
        let (w, h) = (4u32, 4u32);
        let mut src = vec![0u8; (w * h * 3) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 3;
                src[i] = if x < 2 { 255 } else { 0 }; // R
                src[i + 1] = 128; // G
                src[i + 2] = if y < 2 { 255 } else { 0 }; // B
            }
        }
        let out = bilinear_resize(&src, w, h, 2, 2);
        // 2x2 块均值：R 左列 255 右列 0；B 上行 255 下行 0；G 全 128
        assert_eq!(channel(&out, 0), vec![255, 0, 255, 0]);
        assert_eq!(channel(&out, 1), vec![128, 128, 128, 128]);
        assert_eq!(channel(&out, 2), vec![255, 255, 0, 0]);
    }

    #[test]
    fn smooth_gradient_has_no_out_of_range() {
        // 512x384 -> 640x480（设备预览的真实路径）：验证长度与输出非退化
        // （全零/全同值说明插值没生效），像素级正确性由上面的已知系数矩阵测试覆盖。
        let (w, h) = (512u32, 384u32);
        let mut src = vec![0u8; (w * h * 3) as usize];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i % 251) as u8;
        }
        let out = bilinear_resize(&src, w, h, 640, 480);
        assert_eq!(out.len(), 640 * 480 * 3);
        let distinct = {
            let mut set = std::collections::HashSet::new();
            set.extend(out.iter().step_by(997).copied());
            set
        };
        assert!(
            distinct.len() > 100,
            "输出应有丰富值域（实测 {} 种）",
            distinct.len()
        );
    }
}

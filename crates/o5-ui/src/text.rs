//! 文本显示宽度估算 / 截断 / 折行（Python `ui.py` `_display_width_units` /
//! `_fit_units` / `wrap_text` 的逐行移植）。

/// 估算字符宽度：CJK 满宽，其他约 0.55 倍（单位 = 正文字号下一格）。
#[inline]
pub fn display_width_units(ch: char) -> f64 {
    let o = ch as u32;
    if o >= 0x2E80 || (0xFF00..=0xFFEF).contains(&o) {
        // CJK / 全角
        1.0
    } else {
        0.55
    }
}

/// 一段文本的总显示宽度（单位数）。
pub fn display_units(text: &str) -> f64 {
    text.chars().map(display_width_units).sum()
}

/// 按显示宽度截断一行文本（超长加省略号「…」），指标区左段用。
pub fn fit_units(text: &str, max_units: f64) -> String {
    let mut cur = 0.0f64;
    let mut out = String::new();
    for ch in text.chars() {
        let w = display_width_units(ch);
        if cur + w > max_units {
            out.push('…');
            return out;
        }
        out.push(ch);
        cur += w;
    }
    out
}

/// 按显示宽度把一行文本折成多行：按 `\n` 分段后逐字符贪心换行
/// （一个 CJK 字超行宽时也保证至少落下第一个字符，不死循环）。
pub fn wrap_text(text: &str, max_units: f64) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut cur_units = 0.0f64;
        let mut cur = String::new();
        for ch in para.chars() {
            let w = display_width_units(ch);
            if !cur.is_empty() && cur_units + w > max_units {
                lines.push(std::mem::take(&mut cur));
                cur_units = 0.0;
            }
            cur.push(ch);
            cur_units += w;
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_units() {
        assert_eq!(display_width_units('中'), 1.0);
        assert_eq!(display_width_units('语'), 1.0);
        assert_eq!(display_width_units('Ａ'), 1.0); // 全角 U+FF21
        assert_eq!(display_width_units('a'), 0.55);
        assert_eq!(display_width_units('…'), 0.55);
        assert_eq!(display_width_units('·'), 0.55);
        // CJK 部首扩展也算满宽
        assert_eq!(display_width_units('\u{2E80}'), 1.0);
        assert_eq!(display_width_units('\u{2E7F}'), 0.55); // 恰在块外
    }

    #[test]
    fn wrap_cjk_greedy() {
        // 5 个汉字，宽 3.0 → 3 + 2
        let l = wrap_text("一二三四五", 3.0);
        assert_eq!(l, vec!["一二三".to_string(), "四五".to_string()]);

        // 混合：CJK 1.0 + ASCII 0.55
        let l = wrap_text("中a中b", 2.0);
        assert_eq!(l, vec!["中a".to_string(), "中b".to_string()]);

        // 单字超行宽：至少落一个字符
        let l = wrap_text("宽宽", 0.5);
        assert_eq!(l, vec!["宽".to_string(), "宽".to_string()]);

        // 空串 / 空白
        assert_eq!(wrap_text("", 3.0), vec!["".to_string()]);

        // 换行符分段
        let l = wrap_text("ab\ncd", 1.0);
        assert_eq!(l, vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()]);
    }

    #[test]
    fn fit_truncates_with_ellipsis() {
        assert_eq!(fit_units("一二三四五", 3.0), "一二三…");
        // 恰好放满不加省略号
        assert_eq!(fit_units("一二三", 3.0), "一二三");
        assert_eq!(fit_units("ab", 2.0), "ab");
        assert_eq!(fit_units("abc", 1.1), "ab…");
    }
}

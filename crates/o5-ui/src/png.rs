//! 手写最小 PNG 编码器（无压缩 zlib store 模式）。
//!
//! PngDumpSink / 画布转储用。不引 png/image 依赖：IHDR + 单 IDAT
//! （zlib 头 + stored deflate 块 + adler32）+ IEND，每行 filter 0。
//! 解码端（测试）按同样规则展开 stored 块即可读回像素。

/// CRC32（PNG chunk 校验；反射多项式 0xEDB88320）。
pub(crate) fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *v = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// adler32（zlib 流尾校验）。
pub(crate) fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// zlib 流（无压缩 stored 块）：`78 01` + 块序列 + adler32 大端。
fn zlib_store(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    out.push(0x78);
    out.push(0x01);
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut chunks = raw.chunks(65535).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            out.push(if last { 1 } else { 0 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(kind);
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// 编码一张 RGB888 图为 PNG 字节。
pub(crate) fn encode_png_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(rgb.len(), width as usize * height as usize * 3, "像素数不符");
    // 每行 filter 字节 0 + 行数据
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgb[y * stride..(y + 1) * stride]);
    }
    let mut out = Vec::with_capacity(raw.len() + 128);
    out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8bit, RGB, deflate, adaptive, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_store(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// 写 PNG 文件（转储路径 /tmp 下，父目录必须已存在）。
pub(crate) fn write_png(path: &std::path::Path, width: u32, height: u32, rgb: &[u8]) {
    let bytes = encode_png_rgb(width, height, rgb);
    if let Err(e) = std::fs::write(path, &bytes) {
        log::warn!("写 PNG 失败 {}: {}", path.display(), e);
    }
}

/// 解码本编码器的输出（测试用）：只支持 filter 0 + stored deflate 块。
#[cfg(test)]
pub(crate) fn decode_png_store(png: &[u8]) -> (u32, u32, Vec<u8>) {
    assert_eq!(&png[0..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut p = 8usize;
    let (mut w, mut h) = (0u32, 0u32);
    let mut idat = Vec::new();
    while p + 8 <= png.len() {
        let len = u32::from_be_bytes([png[p], png[p + 1], png[p + 2], png[p + 3]]) as usize;
        let kind = &png[p + 4..p + 8];
        let data = &png[p + 8..p + 8 + len];
        match kind {
            b"IHDR" => {
                w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                h = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                assert_eq!(data[8], 8, "bit depth");
                assert_eq!(data[9], 2, "color type RGB");
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        p += 12 + len; // len + kind + data + crc
    }
    assert_eq!(&idat[0..2], &[0x78, 0x01], "应为 store 模式 zlib");
    // 展开存储块
    let mut z = 2usize;
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    loop {
        let bfinal = idat[z] & 1;
        let btype = (idat[z] >> 1) & 3;
        assert_eq!(btype, 0, "仅支持 stored 块");
        let len = u16::from_le_bytes([idat[z + 1], idat[z + 2]]) as usize;
        let nlen = u16::from_le_bytes([idat[z + 3], idat[z + 4]]);
        assert_eq!(!len as u16, nlen, "NLEN 校验");
        raw.extend_from_slice(&idat[z + 5..z + 5 + len]);
        z += 5 + len;
        if bfinal == 1 {
            break;
        }
    }
    // adler32 校验
    let adler = u32::from_be_bytes([idat[z], idat[z + 1], idat[z + 2], idat[z + 3]]);
    assert_eq!(adler, adler32(&raw), "adler32 校验");
    // 去 filter 字节
    let stride = w as usize * 3;
    let mut rgb = Vec::with_capacity(stride * h as usize);
    for y in 0..h as usize {
        let row = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
        assert_eq!(row[0], 0, "filter 0");
        rgb.extend_from_slice(&row[1..]);
    }
    (w, h, rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_values() {
        // IEEE CRC32("123456789") = 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn adler32_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"abc"), 0x024D0127);
        // "Wikipedia" = 0x11E60398
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn roundtrip() {
        let w = 13u32;
        let h = 7u32;
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for (i, v) in rgb.iter_mut().enumerate() {
            *v = (i % 251) as u8;
        }
        let png = encode_png_rgb(w, h, &rgb);
        let (w2, h2, rgb2) = decode_png_store(&png);
        assert_eq!((w2, h2), (w, h));
        assert_eq!(rgb, rgb2);
    }

    #[test]
    fn roundtrip_large_spans_multiple_blocks() {
        // 640x480 会跨 15 个 stored 块
        let w = 640u32;
        let h = 480u32;
        let rgb: Vec<u8> = (0..w as usize * h as usize * 3).map(|i| (i % 256) as u8).collect();
        let png = encode_png_rgb(w, h, &rgb);
        let (w2, h2, rgb2) = decode_png_store(&png);
        assert_eq!((w2, h2), (w, h));
        assert_eq!(rgb, rgb2);
        // CRC 尾长度对齐
        assert!(png.len() > rgb.len());
    }
}

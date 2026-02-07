#[cfg(test)]
#[inline]
fn clamp(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

#[cfg(test)]
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = y as i32 - 16;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (clamp(r), clamp(g), clamp(b))
}

#[cfg(test)]
pub fn yuyv_to_rgba(width: u32, height: u32, stride: usize, src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    let mut di = 0usize;
    let w = width as usize;
    for y in 0..height as usize {
        let row = &src[y * stride..];
        for x in (0..w).step_by(2) {
            let i = x * 2;
            let y0 = row[i];
            let u = row[i + 1];
            let y1 = row[i + 2];
            let v = row[i + 3];
            let (r0, g0, b0) = yuv_to_rgb(y0, u, v);
            let (r1, g1, b1) = yuv_to_rgb(y1, u, v);
            out[di] = r0;
            out[di + 1] = g0;
            out[di + 2] = b0;
            out[di + 3] = 255;
            out[di + 4] = r1;
            out[di + 5] = g1;
            out[di + 6] = b1;
            out[di + 7] = 255;
            di += 8;
        }
    }
    out
}

#[cfg(test)]
pub fn nv12_to_rgba(
    width: u32,
    height: u32,
    y_stride: usize,
    uv_stride: usize,
    src: &[u8],
) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    let w = width as usize;
    let h = height as usize;
    let y_plane = &src[..y_stride * h];
    let uv_plane = &src[y_stride * h..];
    let mut di = 0usize;
    for y in 0..h {
        let y_row = &y_plane[y * y_stride..];
        let uv_row = &uv_plane[(y / 2) * uv_stride..];
        for (x, &yv) in y_row[..w].iter().enumerate() {
            let uv_i = (x / 2) * 2;
            let u = uv_row[uv_i];
            let v = uv_row[uv_i + 1];
            let (r, g, b) = yuv_to_rgb(yv, u, v);
            out[di] = r;
            out[di + 1] = g;
            out[di + 2] = b;
            out[di + 3] = 255;
            di += 4;
        }
    }
    out
}

#[cfg(any(target_os = "windows", test))]
pub fn bgra_to_rgba(width: u32, height: u32, stride: usize, src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    let w = width as usize;
    let mut di = 0usize;
    for y in 0..height as usize {
        let row = &src[y * stride..];
        for x in 0..w {
            let i = x * 4;
            out[di] = row[i + 2];
            out[di + 1] = row[i + 1];
            out[di + 2] = row[i];
            out[di + 3] = 255;
            di += 4;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_black_white() {
        let src = [16u8, 128, 235, 128];
        let out = yuyv_to_rgba(2, 1, 4, &src);
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn nv12_black() {
        let src = [16u8, 16, 16, 16, 128, 128];
        let out = nv12_to_rgba(2, 2, 2, 2, &src);
        assert!(out.chunks_exact(4).all(|px| px == [0, 0, 0, 255]));
    }

    #[test]
    fn bgra_swap() {
        let src = [10u8, 20, 30, 255];
        let out = bgra_to_rgba(1, 1, 4, &src);
        assert_eq!(&out[0..4], &[30, 20, 10, 255]);
    }
}

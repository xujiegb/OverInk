#![forbid(unsafe_code)]

use super::{Encoding, Image};
use ::png::{BitDepth, ColorType, Encoder, Info, SrgbRenderingIntent};
use std::io::Write;

impl Image {
    pub fn write_png(&self, output: impl Write, hdr: bool, sdr_white: f32) -> Result<(), String> {
        if hdr && self.encoding != Encoding::LinearRgba16 {
            return Err("The capture backend did not return HDR pixels".into());
        }
        let mut info = Info::with_size(self.width, self.height);
        info.color_type = ColorType::Rgb;
        if hdr {
            info.bit_depth = BitDepth::Sixteen;
        }
        let write = || -> Result<(), ::png::EncodingError> {
            let mut encoder = Encoder::with_info(output, info)?;
            if !hdr {
                encoder.set_source_srgb(SrgbRenderingIntent::RelativeColorimetric);
            }
            let mut writer = encoder.write_header()?;
            if hdr {
                // ITU-T H.273: BT.2020 primaries, ST 2084 PQ, full-range RGB.
                writer.write_chunk(::png::chunk::cICP, &[9, 16, 0, 1])?;
            }
            let mut stream = writer.stream_writer()?;
            let width = self.width as usize;
            let stride = width * self.encoding.bytes_per_pixel();
            let rows = self.rgba.chunks_exact(stride);
            // Sample neighboring rows before enabling PQ reuse. Detailed
            // captures can stay on the original conversion loop.
            let reuse_pq = hdr
                && self
                    .rgba
                    .chunks_exact(stride)
                    .zip(self.rgba.chunks_exact(stride).skip(1))
                    .step_by((self.height as usize / 16).max(1))
                    .filter(|(a, b)| a == b)
                    .nth(1)
                    .is_some();
            if reuse_pq {
                let mut row = vec![[0; 6]; width];
                let mut previous = &[][..];
                for pixels in rows {
                    let pixels = pixels.as_chunks::<8>().0;
                    if pixels != previous {
                        pq_row(pixels, previous, &mut row);
                    }
                    stream.write_all(row.as_flattened())?;
                    previous = pixels;
                }
            } else {
                let mut row = Vec::with_capacity(width * if hdr { 6 } else { 3 });
                for pixels in rows {
                    row.clear();
                    if self.encoding == Encoding::Srgb8 {
                        for pixel in pixels.as_chunks::<4>().0 {
                            row.extend_from_slice(&pixel[..3]);
                        }
                    } else {
                        for pixel in pixels.as_chunks::<8>().0 {
                            let rgb = linear_rgb(pixel);
                            if hdr {
                                for channel in rec2020(rgb) {
                                    row.extend_from_slice(&pq(channel).to_be_bytes());
                                }
                            } else {
                                row.extend(rgb.map(|channel| {
                                    (crate::color::encode((channel / sdr_white).clamp(0., 1.))
                                        * 255.)
                                        .round() as u8
                                }));
                            }
                        }
                    }
                    stream.write_all(&row)?;
                }
            }
            stream.finish()?;
            writer.finish()
        };
        write().map_err(|error| error.to_string())
    }
}

fn linear_rgb(pixel: &[u8; 8]) -> [f32; 3] {
    [0, 2, 4].map(|i| binary16(u16::from_ne_bytes([pixel[i], pixel[i + 1]])))
}

/// Reuse exact PQ results from the preceding row or the preceding pixel.
/// The streaming encoder already needs this row buffer. Alpha is discarded.
fn pq_row(pixels: &[[u8; 8]], previous: &[[u8; 8]], row: &mut [[u8; 6]]) {
    // Sample the row before paying for a reuse check on every pixel.
    // Detailed capture rows can go straight through the scalar encoder.
    let step = (pixels.len() / 16).max(1);
    let repeated = (0..pixels.len())
        .step_by(step)
        .filter(|&x| {
            let rgb = &pixels[x][..6];
            previous.get(x).is_some_and(|old| old[..6] == *rgb)
                || pixels.get(x + 1).is_some_and(|next| next[..6] == *rgb)
        })
        .nth(1)
        .is_some();
    if !repeated {
        for (pixel, encoded) in pixels.iter().zip(row) {
            *encoded = pq_pixel(pixel);
        }
        return;
    }
    let mut last = None;
    for (x, (pixel, encoded)) in pixels.iter().zip(row).enumerate() {
        let rgb = &pixel[..6];
        if let Some((_, value)) = last.filter(|(key, _)| *key == rgb) {
            *encoded = value;
        } else if !previous.get(x).is_some_and(|old| old[..6] == *rgb) {
            *encoded = pq_pixel(pixel);
        }
        last = Some((rgb, *encoded));
    }
}

fn pq_pixel(pixel: &[u8; 8]) -> [u8; 6] {
    let [[r0, r1], [g0, g1], [b0, b1]] =
        rec2020(linear_rgb(pixel)).map(|channel| pq(channel).to_be_bytes());
    [r0, r1, g0, g1, b0, b1]
}

fn binary16(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1. } else { -1. };
    let exponent = i32::from((bits >> 10) & 31);
    let mantissa = f32::from(bits & 1023);
    match exponent {
        0 => sign * mantissa * (1. / 16_777_216.),
        31 if bits == 0x7c00 => 65_504.,
        31 => 0.,
        _ => {
            let scale = f32::from_bits(((exponent + 112) as u32) << 23);
            sign * (1. + mantissa / 1024.) * scale
        }
    }
}

fn rec2020([r, g, b]: [f32; 3]) -> [f32; 3] {
    // ITU-R BT.2087 linear BT.709 (sRGB primaries) → BT.2020.
    [
        0.627_404 * r + 0.329_283 * g + 0.043_313 * b,
        0.069_097 * r + 0.919_540 * g + 0.011_362 * b,
        0.016_391 * r + 0.088_013 * g + 0.895_595 * b,
    ]
}

fn pq(relative: f32) -> u16 {
    // Windows scRGB uses 80 cd/m² at 1.0. macOS extended linear sRGB
    // is relative to diffuse white, mapped to ITU-R BT.2408's 203 cd/m².
    let reference_white = if cfg!(target_os = "windows") {
        80.
    } else {
        203.
    };
    let luminance = (relative * reference_white / 10_000.).clamp(0., 1.);
    let power = luminance.powf(2610. / 16384.);
    let signal = ((3424. / 4096. + (2413. / 128.) * power) / (1. + (2392. / 128.) * power))
        .powf(2523. / 32.);
    (signal * f32::from(u16::MAX)).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_fp16_infinity_encodes_as_a_highlight() {
        assert_eq!(pq(binary16(0x7c00)), u16::MAX);
        assert_eq!(binary16(0x7e00), 0.);
    }

    #[test]
    fn hdr_png_tags_pq_and_preserves_values_above_diffuse_white() -> Result<(), String> {
        let mut image = Image::hdr_buffer(2, 1)?;
        for (i, sample) in [0x3c00_u16, 0x4400].into_iter().enumerate() {
            for channel in 0..3 {
                let offset = i * 8 + channel * 2;
                image.rgba[offset..offset + 2].copy_from_slice(&sample.to_ne_bytes());
            }
        }
        let mut data = Vec::new();
        image.write_png(&mut data, true, 1.)?;
        let mut reader = ::png::Decoder::new(std::io::Cursor::new(data))
            .read_info()
            .map_err(|error| error.to_string())?;
        assert_eq!(reader.info().bit_depth, BitDepth::Sixteen);
        let metadata = reader
            .info()
            .coding_independent_code_points
            .as_ref()
            .ok_or("PNG is missing cICP metadata")?;
        assert_eq!(metadata.transfer_function, 16);
        let mut output = [0; 12];
        reader
            .next_frame(&mut output)
            .map_err(|error| error.to_string())?;
        let [white, highlight] = [0, 6].map(|i| u16::from_be_bytes([output[i], output[i + 1]]));
        assert_eq!(white, pq(1.));
        assert!(highlight > white);
        Ok(())
    }

    #[test]
    fn hdr_png_matches_per_pixel_encoding_when_colors_repeat() -> Result<(), String> {
        let a = [0x3c00_u16, 0x4400, 0x3800, 0];
        let b = [0x7c00, 0x7e00, 0xbc00, 0x3c00];
        let c = [0, 0x0001, 0x3400, 0x3c00];
        let repeated = [[a[0], a[1], a[2], 0x3c00], c, c, b];
        let source = [[a, a, c, b], repeated, repeated, repeated];
        let mut image = Image::hdr_buffer(4, 4)?;
        for (pixel, source) in image
            .rgba
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(source.as_flattened())
        {
            for (bytes, &bits) in pixel.as_chunks_mut::<2>().0.iter_mut().zip(source) {
                *bytes = bits.to_ne_bytes();
            }
        }
        let mut data = Vec::new();
        image.write_png(&mut data, true, 1.)?;
        let mut reader = ::png::Decoder::new(std::io::Cursor::new(data))
            .read_info()
            .map_err(|error| error.to_string())?;
        let mut output = [0; 4 * 4 * 6];
        reader
            .next_frame(&mut output)
            .map_err(|error| error.to_string())?;
        for (&[r, g, b, _], encoded) in source.as_flattened().iter().zip(output.as_chunks::<6>().0)
        {
            let expected = rec2020([r, g, b].map(binary16)).map(|value| pq(value).to_be_bytes());
            assert_eq!(encoded, expected.as_flattened());
        }
        Ok(())
    }
}

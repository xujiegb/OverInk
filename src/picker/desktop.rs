#![forbid(unsafe_code)]

use crate::{background::Image, color::InkColor, sdl::Window};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        ConnectionExt as _, Format, ImageFormat, ImageOrder, VisualClass, Visualtype,
    },
    rust_connection::RustConnection,
};

#[derive(Clone, Copy, Debug)]
pub struct ScreenPoint {
    root: u32,
    pixels: [i16; 2],
}

fn connect() -> Result<RustConnection, String> {
    RustConnection::connect(None)
        .map(|(connection, _)| connection)
        .map_err(|error| format!("Cannot connect to the X11 display: {error}"))
}

pub fn is_xwayland() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
    {
        return true;
    }
    connect().is_ok_and(|connection| {
        connection
            .query_extension(b"XWAYLAND")
            .ok()
            .and_then(|request| request.reply().ok())
            .is_some_and(|reply| reply.present)
    })
}

fn scaled_pixels(
    local: [f32; 2],
    logical: [f32; 2],
    physical: [i32; 2],
) -> Result<[i16; 2], String> {
    let mut pixels = [0; 2];
    for i in 0..2 {
        if !local[i].is_finite() || !logical[i].is_finite() || logical[i] <= 0. || physical[i] <= 0
        {
            return Err("Invalid screen coordinate or window size".into());
        }
        let coordinate = (local[i] * physical[i] as f32 / logical[i]).floor();
        if !(f32::from(i16::MIN)..=f32::from(i16::MAX)).contains(&coordinate) {
            return Err("Screen coordinate is out of X11 range".into());
        }
        pixels[i] = coordinate as i16;
    }
    Ok(pixels)
}

fn translate(
    connection: &RustConnection,
    window: &Window,
    local: [f32; 2],
) -> Result<ScreenPoint, String> {
    let (logical, physical) = window
        .capture_coordinate_sizes()
        .ok_or("Cannot pick from a zero-sized or unavailable window")?;
    let [x, y] = scaled_pixels(local, logical, physical)?;
    let id = u32::try_from(window.x11_window_number()).map_err(|_| "Invalid X11 window ID")?;
    let root = connection
        .get_geometry(id)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("Cannot locate the canvas on the display: {error}"))?
        .root;
    let reply = connection
        .translate_coordinates(id, root, x, y)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("Cannot convert the selected screen coordinate: {error}"))?;
    if !reply.same_screen {
        return Err("The selected position is on a different X11 screen".into());
    }
    Ok(ScreenPoint {
        root,
        pixels: [reply.dst_x, reply.dst_y],
    })
}

pub fn position(window: &Window, local: [f32; 2]) -> Result<ScreenPoint, String> {
    translate(&connect()?, window, local)
}

#[derive(Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    max: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        let shift = mask.trailing_zeros();
        Self {
            mask,
            shift,
            max: mask >> shift,
        }
    }

    fn normalized(self, pixel: u32) -> f32 {
        ((pixel & self.mask) >> self.shift) as f32 / self.max as f32
    }

    fn byte(self, pixel: u32) -> u8 {
        let value = u64::from((pixel & self.mask) >> self.shift);
        ((value * 255 + u64::from(self.max / 2)) / u64::from(self.max)) as u8
    }
}

struct PixelLayout {
    pixel_bytes: usize,
    stride: usize,
    order: ImageOrder,
    channels: [Channel; 3],
}

impl PixelLayout {
    fn new(
        width: u16,
        format: &Format,
        order: ImageOrder,
        visual: &Visualtype,
    ) -> Result<Self, String> {
        let bits = format.bits_per_pixel;
        let pad = format.scanline_pad;
        if !matches!(bits, 8 | 16 | 24 | 32)
            || !matches!(pad, 8 | 16 | 32)
            || !matches!(order, ImageOrder::LSB_FIRST | ImageOrder::MSB_FIRST)
        {
            return Err("Unsupported X11 pixel format".into());
        }
        let masks = [visual.red_mask, visual.green_mask, visual.blue_mask];
        let [red, green, blue] = masks;
        let used = red | green | blue;
        if !matches!(
            visual.class,
            VisualClass::TRUE_COLOR | VisualClass::DIRECT_COLOR
        ) || masks.contains(&0)
            || (red & green) | (red & blue) | (green & blue) != 0
            || used.leading_zeros() < u32::from(32 - bits)
        {
            return Err("Screen capture requires an RGB X11 visual".into());
        }
        let channels = masks.map(Channel::new);
        if channels
            .iter()
            .any(|channel| !channel.max.wrapping_add(1).is_power_of_two())
        {
            return Err("Invalid X11 color masks".into());
        }
        let scanline_bits = usize::from(width) * usize::from(bits);
        let align = usize::from(pad);
        Ok(Self {
            pixel_bytes: usize::from(bits / 8),
            stride: scanline_bits.div_ceil(align) * align / 8,
            order,
            channels,
        })
    }

    fn pixel(&self, bytes: &[u8]) -> u32 {
        let mut native = [0; 4];
        if self.order == ImageOrder::LSB_FIRST {
            native[..bytes.len()].copy_from_slice(bytes);
            u32::from_le_bytes(native)
        } else {
            native[4 - bytes.len()..].copy_from_slice(bytes);
            u32::from_be_bytes(native)
        }
    }

    fn rgba(&self, bytes: &[u8]) -> [u8; 4] {
        let value = self.pixel(bytes);
        let [red, green, blue] = self.channels.map(|channel| channel.byte(value));
        [red, green, blue, 255]
    }

    fn image(&self, [width, height]: [u16; 2], mut data: Vec<u8>) -> Result<Image, String> {
        let pixels = self
            .stride
            .checked_mul(usize::from(height))
            .ok_or("X11 screenshot is too large")?;
        if data.len() != pixels {
            return Err("Invalid X11 screenshot size".into());
        }
        let [width, height] = [width, height].map(u32::from);
        if self.pixel_bytes == 4 {
            // 32-bit X11 scanlines require no padding. Reuse the reply buffer.
            let masks = self.channels.map(|channel| channel.mask);
            let (pixels, _) = data.as_chunks_mut::<4>();
            if masks != [0xff0000, 0xff00, 0xff] {
                for pixel in pixels {
                    *pixel = self.rgba(pixel);
                }
            } else if self.order == ImageOrder::LSB_FIRST {
                for pixel in pixels {
                    pixel.swap(0, 2);
                    pixel[3] = 255;
                }
            } else {
                for pixel in pixels {
                    pixel.rotate_left(1);
                    pixel[3] = 255;
                }
            }
            return Image::from_rgba8(width, height, data);
        }
        let mut image = Image::buffer(width, height)?;
        let active = width as usize * self.pixel_bytes;
        for (source, target) in data
            .chunks_exact(self.stride)
            .zip(image.rgba.chunks_exact_mut(width as usize * 4))
        {
            for (pixel, output) in source[..active]
                .chunks_exact(self.pixel_bytes)
                .zip(target.as_chunks_mut::<4>().0)
            {
                *output = self.rgba(pixel);
            }
        }
        Ok(image)
    }
}

fn readback(
    connection: &RustConnection,
    point: ScreenPoint,
    size: [u16; 2],
) -> Result<(PixelLayout, Vec<u8>), String> {
    let [width, height] = size;
    let [x, y] = point.pixels;
    Image::rgba8_len(width.into(), height.into())?;
    let bounds = connection
        .get_geometry(point.root)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
    if x < 0
        || y < 0
        || i32::from(x) + i32::from(width) > i32::from(bounds.width)
        || i32::from(y) + i32::from(height) > i32::from(bounds.height)
    {
        return Err("Capture is outside the connected display bounds".into());
    }
    let mut reply = connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            point.root,
            x,
            y,
            width,
            height,
            u32::MAX,
        )
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("Cannot read the X11 desktop: {error}"))?;
    let setup = connection.setup();
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .ok_or("Missing X11 pixmap format")?;
    let visuals = setup
        .roots
        .iter()
        .find(|screen| screen.root == point.root)
        .and_then(|screen| {
            screen
                .allowed_depths
                .iter()
                .find(|depth| depth.depth == reply.depth)
        });
    let visual = visuals
        .and_then(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.visual_id == reply.visual)
        })
        .ok_or("Missing X11 screenshot visual")?;
    let layout = PixelLayout::new(width, format, setup.image_byte_order, visual)?;
    let bytes = layout
        .stride
        .checked_mul(usize::from(height))
        .ok_or("X11 screenshot is too large")?;
    // X11 pads the entire reply to 32 bits in addition to row padding.
    if reply.data.len() != bytes.next_multiple_of(4) {
        return Err("Invalid X11 screenshot size".into());
    }
    reply.data.truncate(bytes);
    Ok((layout, reply.data))
}

pub fn sample(window: &Window, point: ScreenPoint) -> Result<InkColor, String> {
    let (layout, data) = readback(&connect()?, point, [1, 1])?;
    let pixel = layout.pixel(&data[..layout.pixel_bytes]);
    let rgb = layout.channels.map(|channel| channel.normalized(pixel));
    Ok(crate::icc::display_sample_to_srgb(window, rgb))
}

pub fn capture(window: &Window) -> Result<Image, String> {
    let connection = connect()?;
    let origin = translate(&connection, window, [0.; 2])?;
    let [w, h] = window.pixel_size();
    let width = u16::try_from(w).map_err(|_| "Invalid X11 desktop capture width")?;
    let height = u16::try_from(h).map_err(|_| "Invalid X11 desktop capture height")?;
    let (layout, data) = readback(&connection, origin, [width, height])?;
    let mut image = layout.image([width, height], data)?;
    if let Some(profile) = window.icc_profile() {
        image.convert_to_srgb(&profile);
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_and_rejects_invalid_window_dimensions() {
        assert_eq!(
            scaled_pixels([10.5, 11.], [100., 100.], [200, 150]),
            Ok([21, 16])
        );
        assert!(scaled_pixels([3., 4.], [0., 100.], [200, 200]).is_err());
        assert!(scaled_pixels([3., 4.], [100., 100.], [0, 200]).is_err());
        assert!(scaled_pixels([f32::NAN, 4.], [100., 100.], [200, 200]).is_err());
        assert!(scaled_pixels([32_768., 4.], [100., 100.], [100, 100]).is_err());
    }

    #[test]
    fn converts_padded_rows_and_both_x11_byte_orders() -> Result<(), String> {
        let mut visual = Visualtype {
            class: VisualClass::TRUE_COLOR,
            red_mask: 0xff0000,
            green_mask: 0xff00,
            blue_mask: 0xff,
            ..Visualtype::default()
        };
        for (order, source) in [
            (ImageOrder::LSB_FIRST, [51, 34, 17, 0xde, 102, 85, 68, 0xad]),
            (ImageOrder::MSB_FIRST, [17, 34, 51, 0xde, 68, 85, 102, 0xad]),
        ] {
            for bits_per_pixel in [24, 32] {
                let format = Format {
                    depth: 24,
                    bits_per_pixel,
                    scanline_pad: 32,
                };
                let layout = PixelLayout::new(1, &format, order, &visual)?;
                let mut data = source.to_vec();
                if bits_per_pixel == 32 && order == ImageOrder::MSB_FIRST {
                    for pixel in data.as_chunks_mut::<4>().0 {
                        pixel.rotate_right(1);
                    }
                }
                assert!(layout.image([1, 2], vec![0; 7]).is_err());
                let result = layout.image([1, 2], data)?;
                assert_eq!(result.rgba, [17, 34, 51, 255, 68, 85, 102, 255]);
            }
        }
        visual.red_mask = 0xf800;
        visual.green_mask = 0x07e0;
        visual.blue_mask = 0x001f;
        let rgb565 = Format {
            depth: 16,
            bits_per_pixel: 16,
            scanline_pad: 32,
        };
        let layout = PixelLayout::new(1, &rgb565, ImageOrder::LSB_FIRST, &visual)?;
        assert_eq!(layout.rgba(&[0x1f, 0xf8]), [255, 0, 255, 255]);

        visual.red_mask = 0x3ff00000;
        visual.green_mask = 0x000ffc00;
        visual.blue_mask = 0x000003ff;
        let rgb10 = Format {
            depth: 30,
            bits_per_pixel: 32,
            scanline_pad: 32,
        };
        let layout = PixelLayout::new(1, &rgb10, ImageOrder::LSB_FIRST, &visual)?;
        assert_eq!(
            layout.rgba(&0x3ff8_0000_u32.to_le_bytes()),
            [255, 128, 0, 255]
        );
        visual.red_mask = 0;
        assert!(PixelLayout::new(1, &rgb10, ImageOrder::LSB_FIRST, &visual).is_err());
        Ok(())
    }
}

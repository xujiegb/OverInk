use super::{
    Encoding, Image,
    screenshot::{CaptureArea, Crop},
    system::Job,
};
use crate::{native::windows_keyboard::Apartment, sdl::*};
use ::windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::S_OK,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT},
                IDXGIDevice,
            },
            Gdi::HMONITOR,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED,
        },
    },
    core::{IInspectable, Interface, factory},
};
use std::{
    error::Error,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub fn start<T: Send + 'static>(
    display: u32,
    linear: bool,
    crop: Option<Crop>,
    prepare: impl FnOnce(Image) -> Result<T, String> + Send + 'static,
) -> Result<Job<T>, String> {
    let handle =
        windows_monitor_handle(display).ok_or("The Windows display handle is unavailable")?;
    Job::start(
        move |cancelled| {
            capture(
                HMONITOR(ptr::with_exposed_provenance_mut(handle)),
                cancelled,
                linear,
                crop,
            )
            .map_err(|error| format!("Windows desktop capture: {error}"))
        },
        prepare,
    )
}

pub fn hdr_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // SDL may already have initialized the UI thread's COM apartment.
        let Ok(_apartment) = Apartment::enter(RO_INIT_SINGLETHREADED)
            .or_else(|_| Apartment::enter(RO_INIT_MULTITHREADED))
        else {
            return false;
        };
        GraphicsCaptureSession::IsSupported().unwrap_or(false)
            && factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().is_ok()
    })
}

fn capture(
    monitor: HMONITOR,
    cancelled: &AtomicBool,
    linear: bool,
    crop: Option<Crop>,
) -> Result<Option<Image>, Box<dyn Error>> {
    let _apartment = Apartment::enter(RO_INIT_MULTITHREADED)?;
    unsafe {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err("Windows Graphics Capture is unavailable".into());
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
        let size = item.Size()?;
        let area = CaptureArea::new([size.Width.try_into()?, size.Height.try_into()?], crop)?;
        let [width, height] = area.dimensions();
        let encoding = if linear {
            Encoding::LinearRgba16
        } else {
            Encoding::Srgb8
        };
        Image::byte_len(width, height, encoding)?;
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.ok_or("Could not create the capture device")?;
        let context = context.ok_or("Could not create the capture context")?;
        let dxgi: IDXGIDevice = device.cast()?;
        let capture_device: IDirect3DDevice =
            CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        // Capture FP16 scRGB only for an actual HDR/linear output. Otherwise
        // request SDR BGRA8: this works with a transparent SDL renderer and
        // avoids allocating an 8-byte-per-pixel frame on Windows ARM.
        let format = if linear {
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        };
        // This job consumes one frame. Keep a single capture buffer when the
        // driver accepts it and preserve the two-buffer fallback.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&capture_device, format, 1, size)
            .or_else(|_| {
                Direct3D11CaptureFramePool::CreateFreeThreaded(&capture_device, format, 2, size)
            })?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let (ready, frames) = mpsc::sync_channel(1);
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
                let _ = ready.try_send(());
                Ok(())
            });
        let token = pool.FrameArrived(&handler)?;
        let result = (|| {
            session.StartCapture()?;
            let deadline = Instant::now() + Duration::from_secs(8);
            while !cancelled.load(Ordering::Relaxed) {
                if Instant::now() >= deadline {
                    return Err("No desktop frame was received".into());
                }
                match frames.recv_timeout(Duration::from_millis(25)) {
                    Ok(()) => {
                        let frame = match pool.TryGetNextFrame() {
                            Ok(frame) => frame,
                            // windows-rs represents an empty WinRT interface
                            // as an error with a successful HRESULT.
                            Err(error) if error.code() == S_OK => continue,
                            Err(error) => return Err(error.into()),
                        };
                        let copied = (|| {
                            if cancelled.load(Ordering::Relaxed) {
                                return Ok(None);
                            }
                            if frame.ContentSize()? != size {
                                return Err("The display size changed during capture".into());
                            }
                            let mut image = Image::allocate(width, height, encoding)?;
                            copy_frame(&device, &context, &frame, &mut image, area)?;
                            Ok::<_, Box<dyn Error>>(Some(image))
                        })();
                        let _ = frame.Close();
                        return copied;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("The frame pool stopped".into());
                    }
                }
            }
            Ok(None)
        })();
        let _ = pool.RemoveFrameArrived(token);
        let _ = session.Close();
        let _ = pool.Close();
        result
    }
}

fn copy_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
    image: &mut Image,
    area: CaptureArea,
) -> Result<(), Box<dyn Error>> {
    unsafe {
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let source: ID3D11Texture2D = access.GetInterface()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut desc);
        let expected = match image.encoding {
            Encoding::LinearRgba16 => DXGI_FORMAT_R16G16B16A16_FLOAT,
            Encoding::Srgb8 => DXGI_FORMAT_B8G8R8A8_UNORM,
        };
        if desc.Format != expected || [desc.Width, desc.Height] != area.full {
            return Err("The capture did not return the requested desktop frame format".into());
        }
        let [left, top, right, bottom] = area.bounds;
        let source_box = D3D11_BOX {
            left,
            top,
            front: 0,
            right,
            bottom,
            back: 1,
        };
        desc.Width = image.width;
        desc.Height = image.height;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.ok_or("Could not allocate the frame readback texture")?;
        if area.dimensions() == area.full {
            context.CopyResource(&staging, &source);
        } else {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &source, 0, Some(&source_box));
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let stride = mapped.RowPitch as usize;
        let pixels = mapped.pData.cast::<u8>();
        let result = image
            .capture_row_layout(stride)
            .map(|(_, len)| len)
            .filter(|size| !pixels.is_null() && *size <= isize::MAX as usize)
            .ok_or_else(|| "Invalid mapped desktop frame".to_string())
            .and_then(|size| {
                // SAFETY: D3D11 Map returned a live staging allocation.
                // The checked length ends at the last readable pixel.
                // Unmap runs only after the row copy finishes.
                // This is the single boundary producing a borrowed slice.
                let mapped = std::slice::from_raw_parts(pixels, size);
                let mut copy = |offset: usize, row: &mut [u8]| {
                    row.copy_from_slice(&mapped[offset..offset + row.len()]);
                };
                match image.encoding {
                    Encoding::LinearRgba16 => image.copy_hdr_rows(size, stride, &mut copy),
                    Encoding::Srgb8 => image.copy_sdr_bgra_rows(size, stride, &mut copy),
                }
            });
        context.Unmap(&staging, 0);
        result.map_err(Into::into)
    }
}

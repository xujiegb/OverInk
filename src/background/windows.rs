use super::{Image, system::Job};
use crate::sdl::*;
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
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::{Common::DXGI_FORMAT_R16G16B16A16_FLOAT, IDXGIDevice},
            Gdi::HMONITOR,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::{IInspectable, Interface, factory},
};
use std::{
    error::Error,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub fn start(display: u32) -> Result<Job, String> {
    let monitor = unsafe {
        SDL_GetPointerProperty(
            SDL_GetDisplayProperties(SDL_DisplayID(display)),
            SDL_PROP_DISPLAY_WINDOWS_HMONITOR_POINTER,
            ptr::null_mut(),
        )
    };
    if monitor.is_null() {
        return Err("The Windows display handle is unavailable".into());
    }
    let handle = monitor as usize;
    Job::start(move |cancelled| {
        capture(HMONITOR(handle as *mut _), cancelled)
            .map_err(|error| format!("Windows floating-point desktop capture: {error}"))
    })
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() }
    }
}

fn capture(monitor: HMONITOR, cancelled: &AtomicBool) -> Result<Option<Image>, Box<dyn Error>> {
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
        let _apartment = Apartment;
        if !GraphicsCaptureSession::IsSupported()? {
            return Err("Windows Graphics Capture is unavailable".into());
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
        let size = item.Size()?;
        let mut image = Image::hdr_buffer(size.Width.try_into()?, size.Height.try_into()?)?;
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
        // The WGC FP16 format is linear scRGB. Its reference is 80 nits,
        // including when the user's SDR white level differs from that value.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &capture_device,
            DirectXPixelFormat::R16G16B16A16Float,
            2,
            size,
        )?;
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
                        let frame = pool.TryGetNextFrame()?;
                        let size = frame.ContentSize()?;
                        if size.Width != image.width as i32 || size.Height != image.height as i32 {
                            return Err("The display size changed during capture".into());
                        }
                        let copied = copy_frame(&device, &context, &frame, &mut image);
                        let _ = frame.Close();
                        copied?;
                        return Ok(Some(image));
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
) -> Result<(), Box<dyn Error>> {
    unsafe {
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let source: ID3D11Texture2D = access.GetInterface()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut desc);
        if desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT
            || desc.Width != image.width
            || desc.Height != image.height
        {
            return Err("The capture did not return the requested FP16 desktop frame".into());
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.ok_or("Could not allocate the frame readback texture")?;
        context.CopyResource(&staging, &source);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let stride = mapped.RowPitch as usize;
        let result = stride
            .checked_mul(image.height as usize)
            .filter(|size| *size <= isize::MAX as usize)
            .ok_or_else(|| "Invalid mapped desktop frame".to_string())
            .and_then(|size| image.copy_hdr_rows(mapped.pData.cast(), size, stride));
        context.Unmap(&staging, 0);
        result.map_err(Into::into)
    }
}

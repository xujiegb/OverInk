use super::PickResult;
use crate::color::{InkColor, Space};
use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSColorSampler, NSColorSpace};
use std::sync::mpsc;

pub struct SystemJob {
    _sampler: Retained<NSColorSampler>,
    _callback: block2::RcBlock<dyn Fn(*mut NSColor)>,
    receiver: mpsc::Receiver<PickResult>,
}
impl SystemJob {
    pub fn start() -> Result<Self, String> {
        unsafe {
            let (sender, receiver) = mpsc::channel();
            let sampler = NSColorSampler::new();
            let callback = block2::RcBlock::new(move |color: *mut NSColor| {
                let result = if let Some(color) = color.as_ref() {
                    match color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) {
                        Some(rgb) => Ok(Some(InkColor {
                            rgb: [
                                rgb.redComponent() as f32,
                                rgb.greenComponent() as f32,
                                rgb.blueComponent() as f32,
                            ]
                            .map(|v| v.clamp(0., 1.)),
                            space: Space::Srgb,
                        })),
                        None => Err("Cannot convert the sampled color to sRGB".into()),
                    }
                } else {
                    Ok(None)
                };
                let _ = sender.send(result);
            });
            sampler.showSamplerWithSelectionHandler(&callback);
            Ok(Self {
                _sampler: sampler,
                _callback: callback,
                receiver,
            })
        }
    }
    pub fn poll(&mut self) -> Option<PickResult> {
        match self.receiver.try_recv() {
            Ok(v) => Some(v),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("System color sampler disconnected".into())),
        }
    }
    pub fn cancel(&mut self) {}
}

//! Runtime-loaded NVIDIA encoder. Linear RGB readback supports hybrid GPUs.
use crate::codec::DmaFrame;
use anyhow::{Result, ensure};
use moq_nvenc::{
    Bitstream, Buffer, EncodePictureParams, EncoderInitParams, Session, sys::nvEncodeAPI::*,
};
use std::os::fd::AsRawFd;

pub struct Encoder {
    input: Option<Buffer>,
    egl: Option<std::sync::Arc<crate::nvenc_egl::Display>>,
    context: std::sync::Arc<cudarc::driver::CudaContext>,
    output: Option<Bitstream>,
    session: Session,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}
impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, node: &str) -> Result<Self> {
        // Probe first: cudarc's dynamic loader panics when CUDA is absent.
        let _cuda = unsafe { libloading::Library::new("libcuda.so.1")? };
        moq_nvenc::Encoder::load()?;
        let context = cudarc::driver::CudaContext::new(0)
            .map_err(|e| anyhow::anyhow!("CUDA context: {e:?}"))?;
        let encoder = moq_nvenc::Encoder::initialize_with_cuda(context.clone())?;
        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        let mut cfg = encoder
            .get_preset_config(NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P1_GUID, tuning)?
            .presetCfg;
        cfg.gopLength = u32::MAX;
        cfg.frameIntervalP = 1;
        cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        cfg.rcParams.averageBitRate = 10_000_000;
        cfg.rcParams.maxBitRate = 10_000_000;
        cfg.rcParams.vbvBufferSize = 10_000_000 / fps;
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        cfg.rcParams.set_enableLookahead(0);
        unsafe {
            cfg.encodeCodecConfig.h264Config.idrPeriod = u32::MAX;
            cfg.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
        }
        let mut init = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, width, height);
        init.preset_guid(NV_ENC_PRESET_P1_GUID)
            .tuning_info(tuning)
            .framerate(fps, 1)
            .enable_picture_type_decision();
        unsafe {
            init.encode_config(cfg);
        }
        let session =
            encoder.start_session(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB, init)?;
        let input = Some(session.create_input_buffer()?);
        let output = Some(session.create_output_bitstream()?);
        let vendor = std::fs::read_to_string(format!(
            "/sys/class/drm/{}/device/vendor",
            node.rsplit('/').next().unwrap_or("")
        ))
        .unwrap_or_default();
        let egl = if vendor.trim() == "0x10de" {
            Some(crate::nvenc_egl::Display::new(node)?)
        } else {
            None
        };
        eprintln!(
            "NVENC input: {}",
            if egl.is_some() {
                "cuda-egl"
            } else {
                "cpu-copy"
            }
        );
        Ok(Self {
            input,
            output,
            session,
            egl,
            context,
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        })
    }
    pub fn bitrate(&mut self, bitrate: u64) -> Result<()> {
        self.session.reconfigure(bitrate.min(10_000_000) as u32)?;
        Ok(())
    }
    pub fn encode(&mut self, src: &DmaFrame, us: u64, key: bool) -> Result<Vec<(bool, Vec<u8>)>> {
        self.context
            .bind_to_thread()
            .map_err(|e| anyhow::anyhow!("CUDA bind: {e:?}"))?;
        if let Some(egl) = &self.egl {
            if src.width == self.width && src.height == self.height && !src.y_inverted {
                let image = egl.import(src)?;
                let array = image.array;
                let input = unsafe {
                    self.session.register_generic_resource(
                        image,
                        NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY,
                        array,
                        0,
                    )?
                };
                let output = self
                    .output
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("NVENC output lost"))?;
                let (bytes, input, output) = self
                    .session
                    .encode_picture(
                        input,
                        output,
                        EncodePictureParams {
                            input_timestamp: us,
                            force_idr: key,
                            ..Default::default()
                        },
                    )?
                    .finish()?;
                drop(input);
                self.output = Some(output);
                let idr = bytes
                    .windows(4)
                    .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == 5);
                return Ok(vec![(idr, bytes)]);
            }
            // Scaling/transforms need a copy; native upright NVIDIA capture stays zero-copy.
            let image = egl.import(src)?;
            let bytes = egl.readback(&image, src.width, src.height)?;
            self.copy_pixels(&bytes, src.width as usize * 4, 0, src);
        } else {
            ensure!(
                src.descriptor.layers.len() == 1 && src.descriptor.objects.len() == 1,
                "NVENC copy requires packed RGB"
            );
            let l = &src.descriptor.layers[0];
            let o = &src.descriptor.objects[0];
            ensure!(
                l.num_planes == 1 && o.drm_format_modifier == 0 && l.object_index[0] == 0,
                "NVENC copy requires linear RGB"
            );
            ensure!(
                matches!(l.drm_format, v if v == u32::from_le_bytes(*b"AR24") || v == u32::from_le_bytes(*b"XR24")),
                "unsupported NVENC copy format"
            );
            ensure!(
                l.pitch[0] >= src.width * 4
                    && u64::from(l.offset[0]) + u64::from(l.pitch[0]) * u64::from(src.height)
                        <= u64::from(o.size),
                "short NVENC capture buffer"
            );
            let map = crate::software::Mapping::new(o.fd.as_raw_fd(), o.size as usize)?;
            let bytes =
                unsafe { std::slice::from_raw_parts(map.ptr.cast::<u8>(), o.size as usize) };
            self.copy_pixels(bytes, l.pitch[0] as usize, l.offset[0] as usize, src);
            drop(map);
        }
        let mut input = self
            .input
            .take()
            .ok_or_else(|| anyhow::anyhow!("NVENC input lost after failed submission"))?;
        {
            let mut lock = input.lock()?;
            let pitch = lock.pitch() as usize;
            ensure!(pitch >= self.width as usize * 4, "short NVENC input pitch");
            unsafe {
                lock.write_rows(
                    0,
                    pitch,
                    &self.pixels,
                    self.width as usize * 4,
                    self.height as usize,
                );
            }
        }
        let output = self
            .output
            .take()
            .ok_or_else(|| anyhow::anyhow!("NVENC output lost after failed submission"))?;
        let (bytes, input, output) = self
            .session
            .encode_picture(
                input,
                output,
                EncodePictureParams {
                    input_timestamp: us,
                    force_idr: key,
                    ..Default::default()
                },
            )?
            .finish()?;
        self.input = Some(input);
        self.output = Some(output);
        let idr = bytes
            .windows(4)
            .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == 5);
        Ok(vec![(idr, bytes)])
    }
    fn copy_pixels(&mut self, bytes: &[u8], stride: usize, offset: usize, src: &DmaFrame) {
        for y in 0..self.height as usize {
            let sy = y * src.height as usize / self.height as usize;
            let sy = if src.y_inverted {
                src.height as usize - 1 - sy
            } else {
                sy
            };
            for x in 0..self.width as usize {
                let i = offset + sy * stride + x * src.width as usize / self.width as usize * 4;
                let j = (y * self.width as usize + x) * 4;
                self.pixels[j..j + 4].copy_from_slice(&bytes[i..i + 4]);
            }
        }
    }
}

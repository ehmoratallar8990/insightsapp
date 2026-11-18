//! CLIP ViT-B/32 running on candle (pure Rust). Uses Metal on macOS when built
//! with `--features metal`, CUDA with `--features cuda`, otherwise the CPU.

use super::tokenizer::{CONTEXT, ClipTokenizer};
use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::clip::{ClipConfig, ClipModel, div_l2_norm};
use std::path::PathBuf;

const REPO: &str = "https://huggingface.co/openai/clip-vit-base-patch32/resolve";
pub const FILES: [(&str, &str); 3] = [
    ("refs%2Fpr%2F15/model.safetensors", "model.safetensors"),
    ("main/vocab.json", "vocab.json"),
    ("main/merges.txt", "merges.txt"),
];

pub fn model_dir() -> PathBuf {
    crate::index::data_dir().join("models").join("clip-vit-base-patch32")
}

pub fn is_downloaded() -> bool {
    FILES.iter().all(|(_, f)| model_dir().join(f).exists())
}

/// Without the `download` feature, model files must be placed in `model_dir()` manually.
#[cfg(not(feature = "download"))]
pub fn download(_progress: impl FnMut(u64, u64)) -> Result<()> {
    anyhow::bail!(
        "built without the `download` feature: put model.safetensors, vocab.json and merges.txt from \
         huggingface.co/openai/clip-vit-base-patch32 into {}",
        model_dir().display()
    )
}

/// Download the model files, reporting `(downloaded, total)` bytes.
#[cfg(feature = "download")]
pub fn download(mut progress: impl FnMut(u64, u64)) -> Result<()> {
    use std::io::{Read, Write};
    let dir = model_dir();
    std::fs::create_dir_all(&dir)?;
    for (remote, local) in FILES {
        let dst = dir.join(local);
        if dst.exists() {
            continue;
        }
        let url = format!("{REPO}/{remote}");
        let resp = ureq::get(&url).call().with_context(|| format!("GET {url}"))?;
        let total = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let mut reader = resp.into_body().into_reader();
        let part = dst.with_extension("part");
        let mut f = std::io::BufWriter::new(std::fs::File::create(&part)?);
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        f.flush()?;
        drop(f);
        std::fs::rename(&part, &dst)?;
    }
    Ok(())
}

pub struct Clip {
    model: ClipModel,
    tokenizer: ClipTokenizer,
    pub device: Device,
}

pub fn best_device() -> Device {
    #[cfg(target_os = "macos")]
    if let Ok(d) = Device::new_metal(0) {
        return d;
    }
    #[cfg(feature = "cuda")]
    if let Ok(d) = Device::new_cuda(0) {
        return d;
    }
    Device::Cpu
}

impl Clip {
    pub fn load() -> Result<Self> {
        let dir = model_dir();
        let device = best_device();
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &device)? };
        let model = ClipModel::new(vb, &ClipConfig::vit_base_patch32())?;
        let tokenizer = ClipTokenizer::load(&dir.join("vocab.json"), &dir.join("merges.txt"))?;
        Ok(Self { model, tokenizer, device })
    }

    pub fn device_name(&self) -> &'static str {
        match self.device {
            Device::Cpu => "CPU",
            Device::Metal(_) => "Apple GPU (Metal)",
            Device::Cuda(_) => "NVIDIA GPU (CUDA)",
        }
    }

    /// L2-normalised image embeddings for a batch of preprocessed CHW images.
    pub fn embed_images(&self, pixels: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
        if pixels.is_empty() {
            return Ok(Vec::new());
        }
        let flat: Vec<f32> = pixels.iter().flatten().copied().collect();
        let t = Tensor::from_vec(flat, (pixels.len(), 3, 224, 224), &self.device)?;
        let f = div_l2_norm(&self.model.get_image_features(&t)?)?;
        Ok(f.to_device(&Device::Cpu)?.to_vec2::<f32>()?)
    }

    /// L2-normalised text embeddings.
    pub fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(64) {
            let ids: Vec<u32> = chunk.iter().flat_map(|t| self.tokenizer.encode(t)).collect();
            let t = Tensor::from_vec(ids, (chunk.len(), CONTEXT), &self.device)?;
            let f = div_l2_norm(&self.model.get_text_features(&t)?)?;
            out.extend(f.to_device(&Device::Cpu)?.to_vec2::<f32>()?);
        }
        Ok(out)
    }
}

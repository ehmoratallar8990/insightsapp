//! A small, dependency-free port of OpenAI's CLIP byte-level BPE tokenizer
//! (`simple_tokenizer.py`), reading the standard `vocab.json` + `merges.txt`.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub const SOT: u32 = 49406;
pub const EOT: u32 = 49407;
pub const CONTEXT: usize = 77;

pub struct ClipTokenizer {
    encoder: HashMap<String, u32>,
    ranks: HashMap<(String, String), u32>,
    byte_enc: [char; 256],
    pattern: regex::Regex,
    cache: parking_lot::Mutex<HashMap<String, Vec<u32>>>,
}

fn bytes_to_unicode() -> [char; 256] {
    let mut bs: Vec<u32> = (b'!' as u32..=b'~' as u32).chain(0xA1..=0xAC).chain(0xAE..=0xFF).collect();
    let mut cs = bs.clone();
    let mut n = 0;
    for b in 0..256u32 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    let mut out = ['\0'; 256];
    for (b, c) in bs.into_iter().zip(cs) {
        out[b as usize] = char::from_u32(c).unwrap();
    }
    out
}

impl ClipTokenizer {
    pub fn load(vocab: &Path, merges: &Path) -> Result<Self> {
        let encoder: HashMap<String, u32> =
            serde_json::from_slice(&std::fs::read(vocab).context("read vocab.json")?).context("parse vocab.json")?;
        let merges_txt = std::fs::read_to_string(merges).context("read merges.txt")?;
        let mut ranks = HashMap::new();
        for (i, line) in merges_txt.lines().skip(1).enumerate() {
            if let Some((a, b)) = line.split_once(' ') {
                ranks.insert((a.to_string(), b.to_string()), i as u32);
            }
        }
        let pattern = regex::Regex::new(
            r"(?i)<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|\p{L}+|\p{N}|[^\s\p{L}\p{N}]+",
        )?;
        Ok(Self { encoder, ranks, byte_enc: bytes_to_unicode(), pattern, cache: Default::default() })
    }

    fn bpe(&self, token: &str) -> Vec<u32> {
        if let Some(v) = self.cache.lock().get(token) {
            return v.clone();
        }
        let chars: Vec<char> = token.chars().collect();
        let mut word: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
        if let Some(last) = word.last_mut() {
            last.push_str("</w>");
        }
        loop {
            if word.len() < 2 {
                break;
            }
            let mut best: Option<(u32, usize)> = None;
            for i in 0..word.len() - 1 {
                if let Some(&r) = self.ranks.get(&(word[i].clone(), word[i + 1].clone())) {
                    if best.is_none_or(|(br, _)| r < br) {
                        best = Some((r, i));
                    }
                }
            }
            let Some((_, _)) = best else { break };
            let (first, second) = {
                let (_, i) = best.unwrap();
                (word[i].clone(), word[i + 1].clone())
            };
            let mut merged = Vec::with_capacity(word.len());
            let mut i = 0;
            while i < word.len() {
                if i + 1 < word.len() && word[i] == first && word[i + 1] == second {
                    merged.push(format!("{first}{second}"));
                    i += 2;
                } else {
                    merged.push(word[i].clone());
                    i += 1;
                }
            }
            word = merged;
        }
        let ids: Vec<u32> = word.iter().filter_map(|w| self.encoder.get(w).copied()).collect();
        self.cache.lock().insert(token.to_string(), ids.clone());
        ids
    }

    pub fn encode(&self, text: &str) -> Vec<u32> {
        let clean = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let mut ids = vec![SOT];
        for m in self.pattern.find_iter(&clean) {
            let mapped: String = m.as_str().bytes().map(|b| self.byte_enc[b as usize]).collect();
            ids.extend(self.bpe(&mapped));
        }
        ids.truncate(CONTEXT - 1);
        ids.push(EOT);
        // Pad with 0 so the EOT token stays the unique arg-max used for pooling.
        ids.resize(CONTEXT, 0);
        ids
    }
}

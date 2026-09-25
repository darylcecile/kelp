//! Bounded copy/insert encoding against one pack-local base. These references
//! are physical compression only, never version-control dependencies.
use std::collections::HashMap;

use anyhow::{Result, ensure};

pub struct Dictionary<'a> {
    base: &'a [u8],
    blocks: HashMap<u64, usize>,
}

fn key(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

impl<'a> Dictionary<'a> {
    pub fn new(base: &'a [u8]) -> Self {
        let mut blocks = HashMap::new();
        for offset in (0..base.len().saturating_sub(15)).step_by(16) {
            blocks
                .entry(key(&base[offset..offset + 16]))
                .or_insert(offset);
        }
        Self { base, blocks }
    }

    pub fn encode(&self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        let (mut offset, mut literal) = (0, 0);
        while offset + 16 <= bytes.len() {
            let matched = self
                .blocks
                .get(&key(&bytes[offset..offset + 16]))
                .copied()
                .filter(|base| self.base[*base..*base + 16] == bytes[offset..offset + 16]);
            if let Some(base) = matched {
                insert(&mut output, &bytes[literal..offset]);
                let mut length = 16;
                while offset + length < bytes.len()
                    && base + length < self.base.len()
                    && bytes[offset + length] == self.base[base + length]
                {
                    length += 1;
                }
                output.push(1);
                output.extend_from_slice(&(base as u32).to_be_bytes());
                output.extend_from_slice(&(length as u32).to_be_bytes());
                offset += length;
                literal = offset;
            } else {
                offset += 1;
            }
        }
        insert(&mut output, &bytes[literal..]);
        output
    }
}

fn insert(output: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    output.push(0);
    output.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    output.extend_from_slice(bytes);
}

pub fn decode(base: &[u8], encoded: &[u8], length: usize) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(length);
    let mut input = encoded;
    while !input.is_empty() {
        let tag = input[0];
        input = &input[1..];
        ensure!(input.len() >= 4, "truncated delta");
        let first = u32::from_be_bytes(input[..4].try_into()?) as usize;
        input = &input[4..];
        match tag {
            0 => {
                ensure!(
                    first <= length - output.len() && first <= input.len(),
                    "invalid delta literal"
                );
                output.extend_from_slice(&input[..first]);
                input = &input[first..];
            }
            1 => {
                ensure!(input.len() >= 4, "truncated delta copy");
                let count = u32::from_be_bytes(input[..4].try_into()?) as usize;
                input = &input[4..];
                ensure!(
                    count <= length - output.len(),
                    "delta exceeds object length"
                );
                let end = first
                    .checked_add(count)
                    .ok_or_else(|| anyhow::anyhow!("delta offset overflow"))?;
                output.extend_from_slice(
                    base.get(first..end)
                        .ok_or_else(|| anyhow::anyhow!("delta outside base"))?,
                );
            }
            _ => anyhow::bail!("invalid delta operation"),
        }
    }
    ensure!(output.len() == length, "delta length mismatch");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_insert_preserves_edits_insertions_deletions_and_binary_bytes() -> Result<()> {
        let base = b"prefix repeated region 0123456789 suffix".repeat(100);
        let dictionary = Dictionary::new(&base);
        let mut changed = base.clone();
        changed.splice(50..70, [0, 255, 1, 2]);
        changed.splice(500..500, b"new literal".iter().copied());
        changed.truncate(changed.len() - 27);
        for data in [
            changed,
            base.clone(),
            vec![],
            vec![255; 3000],
            b"short".to_vec(),
        ] {
            let encoded = dictionary.encode(&data);
            assert_eq!(decode(&base, &encoded, data.len())?, data);
        }
        assert!(dictionary.encode(&base).len() < base.len() / 10);
        assert!(decode(&base, &[1, 255, 255, 255, 255, 0, 0, 0, 16], 16).is_err());
        assert!(decode(&base, &[0, 0, 0, 0, 20, 1], 20).is_err());
        Ok(())
    }
}

//! A tiny thread-safe sample FIFO that connects a source to an output.
//! Latency is capped: if the output falls behind, the oldest audio is dropped.

use std::collections::VecDeque;
use std::sync::Mutex;

pub struct Ring {
    buf: Mutex<VecDeque<f32>>,
    max_samples: usize,
}

impl Ring {
    pub fn new(max_samples: usize) -> Self {
        Ring { buf: Mutex::new(VecDeque::with_capacity(max_samples)), max_samples }
    }

    pub fn push(&self, samples: &[f32]) {
        let mut b = self.buf.lock().unwrap();
        b.extend(samples.iter().copied());
        let excess = b.len().saturating_sub(self.max_samples);
        if excess > 0 {
            b.drain(..excess);
        }
    }

    /// Pop up to `out.len()` samples; returns how many were written.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let mut b = self.buf.lock().unwrap();
        let n = out.len().min(b.len());
        for (dst, src) in out.iter_mut().zip(b.drain(..n)) {
            *dst = src;
        }
        n
    }

    pub fn len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }
}

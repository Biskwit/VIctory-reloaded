//! The title's bytes under flights' writes, by mask (on by default;
//! `VICTORY_UNDER_MASKS=0`: by runs, as before; `VICTORY_AB` switches it
//! while the title runs).
//!
//! Where the title wrote a block (64 KiB) of a copy in the card's memory that
//! a flight's shaders may write, [`crate::shadow::Shadow::merge_under_writes`]
//! finds the words the title changed and sends only those up, so that what
//! the shaders write beside them stays. It made them runs: some 100,000 a
//! frame in some 320 blocks in the Series S cell, most in the title's 174 MB
//! heap, which one view of the skinning passes declares written whole (run
//! land-who-1: the card changed 0.26% of the pieces declared there). Each run went
//! through the copy's list of changed ranges, its join, the sends' notes and
//! a pair of the words' dispatch: some 4 ms a frame of the submitting thread
//! (profile land-prof-main-1). Here each 4 KiB of such a block with a word
//! the title changed is one mask instead, a bit for each word of it, and its
//! words go up whole beside the mask.
//!
//! What a mask promises (a [`Masked`] of a copy, made by the merge):
//! - It is of 4 KiB at most, whole words of the copy, in a range a flight in
//!   the air may write. A bit is set where the
//!   title's word differed from what the copy's CPU side held, and the CPU
//!   side holds the title's word there from then on (where no bit is set it
//!   held it already). The words are those the runs would have been; a word
//!   is taken to be the title's or the shaders', not shared, as before
//!   (`crate::host::send::words_under`).
//! - It is taken with the copy's changed ranges, by the same submission's
//!   `crate::shadow::card_dirty`, numbered as they are (`QUEUE_ORDER`), and
//!   its words come out of the CPU side then (`crate::host::send::take`),
//!   as theirs do.
//!
//! The order on the card's queue: the masked store is in the submission's
//! list of sends, which goes to the queue after every flight sent before it
//! and ahead of the submission's own lists. Every flight that may write
//! those words was in the air when the mask was made, so its shaders' writes are on
//! the card's side before the store; the store writes the words of set bits
//! alone, so every other word keeps what the flight wrote there. A later
//! refresh or mask of the same words goes up with a later submission, behind
//! it.
//! The pieces it lies in are hashed again after it, as those of the runs
//! sent up, so that the submission's own compare starts from them.
//!
//! What a landing may skip: a fetch of the mask's piece may have run before
//! the store or after it (the fetches go to the copy queue). Where a bit is
//! set the word is the title's, and if the store is not known to be ahead of
//! the fetch (the mask not taken yet, or taken by a send numbered after the
//! fetch) the card's word in the fetch is the one from before: the landing
//! leaves those words alone ([`Masks::near`]), as it leaves the runs of
//! `dirty` and of the sent runs. Masks are let go with those runs, once a
//! fetch after their send has been merged.

/// Words of a mask at most (4 KiB).
pub(crate) const WORDS: usize = 1024;

/// Whether the title's words under flights' writes become masks.
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_UNDER_MASKS") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UNDER_MASKS").is_none_or(|v| v != "0"))
}

/// The title's words in 4 KiB at most of a copy: from offset `at` of the
/// copy, `words` words, and a bit for each that is set where the word is the
/// title's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Masked {
    pub(crate) at: usize,
    pub(crate) words: u32,
    pub(crate) mask: [u32; WORDS / 32],
}

impl Masked {
    /// The mask of the words where `title` differs from `held` (one length,
    /// whole words, [`WORDS`] at most), of the part at offset `at`; `None` if
    /// there is none.
    pub(crate) fn of(at: usize, title: &[u8], held: &[u8]) -> Option<Masked> {
        assert!(title.len() == held.len() && title.len().is_multiple_of(4) && title.len() <= WORDS * 4);
        let mut mask = [0u32; WORDS / 32];
        let words = title.len() / 4;
        let mut w = 0;
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the processor has AVX2; both hold `words` words.
            unsafe { mask_avx2(title, held, &mut mask) };
            w = words / 8 * 8;
        }
        for i in w..words {
            if title[4 * i..4 * i + 4] != held[4 * i..4 * i + 4] {
                mask[i / 32] |= 1 << (i % 32);
            }
        }
        mask.iter().any(|&m| m != 0).then_some(Masked { at, words: words as u32, mask })
    }

    /// Its words that are the title's.
    pub(crate) fn count(&self) -> u32 {
        self.mask.iter().map(|m| m.count_ones()).sum()
    }

    /// Adds to `out` the offsets of the copy its set words take, inside
    /// `[from, to)`: runs of whole words, in order.
    fn runs_in(&self, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
        let end = self.at + 4 * self.words as usize;
        if self.at >= to || end <= from {
            return;
        }
        let mut run: Option<(usize, usize)> = None;
        for i in 0..self.words as usize {
            let a = self.at + 4 * i;
            if self.mask[i / 32] >> (i % 32) & 1 != 0 && a + 4 > from && a < to {
                match &mut run {
                    Some(r) if r.1 == a => r.1 = a + 4,
                    _ => {
                        out.extend(run.take());
                        run = Some((a, a + 4));
                    }
                }
            }
        }
        out.extend(run);
    }
}

/// # Safety
/// AVX2; `title` and `held` hold one length of whole words, [`WORDS`] at most.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mask_avx2(title: &[u8], held: &[u8], mask: &mut [u32; WORDS / 32]) {
    use std::arch::x86_64::*;
    let blocks = title.len() / 32;
    for b in 0..blocks {
        // SAFETY: block `b` lies in both.
        let same = unsafe {
            _mm256_cmpeq_epi32(
                _mm256_loadu_si256(title.as_ptr().add(32 * b).cast()),
                _mm256_loadu_si256(held.as_ptr().add(32 * b).cast()),
            )
        };
        let differ = !_mm256_movemask_ps(_mm256_castsi256_ps(same)) as u32 & 0xff;
        mask[b / 4] |= differ << (8 * (b % 4));
    }
}

/// A copy's masks: those not taken yet, and with the relay those taken, by
/// the number of the send that took them. Locked inside the lock of the
/// copy's changed ranges.
#[derive(Default)]
pub(crate) struct Masks {
    dirty: Vec<Masked>,
    sent: Vec<(u64, Vec<Masked>)>,
}

impl Masks {
    /// Masks the merge made.
    pub(crate) fn put(&mut self, pages: Vec<Masked>) {
        self.dirty.extend(pages);
    }

    /// Taken for the send numbered `sent_as`; with the relay (`noted`) kept
    /// as sent by it, those of sends up to `fetched` let go.
    pub(crate) fn take(&mut self, sent_as: u64, noted: bool, fetched: u64) -> Vec<Masked> {
        let pages = std::mem::take(&mut self.dirty);
        if noted {
            self.sent.retain(|s| s.0 > fetched);
            if !pages.is_empty() {
                self.sent.push((sent_as, pages.clone()));
            }
        }
        pages
    }

    /// Adds to `out` the offsets in `[from, to)` that a fetch which ran
    /// behind the sends up to `fetched_after` may hold the card's old words
    /// at: those of masks not taken, and of masks taken by later sends.
    pub(crate) fn near(&self, fetched_after: u64, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
        for page in self.dirty.iter().chain(self.sent.iter().filter(|s| s.0 > fetched_after).flat_map(|s| s.1.iter())) {
            page.runs_in(from, to, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// The mask has a bit for every word that differs and for no other, on
    /// both paths (blocks of eight words and the words behind them).
    #[test]
    fn the_mask_is_the_words_that_differ() {
        let mut seed = 11u32;
        let mut random = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        for words in [1usize, 7, 8, 9, 300, 1023, 1024] {
            let held: Vec<u32> = (0..words).map(|_| random()).collect();
            let mut title = held.clone();
            let mut wanted = [0u32; WORDS / 32];
            for (i, t) in title.iter_mut().enumerate() {
                if random() % 3 == 0 {
                    *t ^= 1 << (random() % 32);
                    wanted[i / 32] |= 1 << (i % 32);
                }
            }
            let got = Masked::of(64, &bytes(&title), &bytes(&held));
            match got {
                Some(m) => assert_eq!((m.at, m.words as usize, m.mask), (64, words, wanted), "{words} words"),
                None => assert!(wanted.iter().all(|&w| w == 0)),
            }
        }
        assert_eq!(Masked::of(0, &bytes(&[5, 6]), &bytes(&[5, 6])), None);
    }

    /// The offsets a mask skips are its set words, in runs, cut to the range
    /// asked about.
    #[test]
    fn a_mask_skips_its_words_in_runs() {
        let mut mask = [0u32; WORDS / 32];
        for i in [0usize, 1, 2, 5, 31, 32, 33, 100] {
            mask[i / 32] |= 1 << (i % 32);
        }
        let page = Masked { at: 0x1000, words: 200, mask };
        let mut out = Vec::new();
        page.runs_in(0, usize::MAX, &mut out);
        assert_eq!(out, [(0x1000, 0x100c), (0x1014, 0x1018), (0x107c, 0x1088), (0x1190, 0x1194)]);
        out.clear();
        page.runs_in(0x1004, 0x1080, &mut out);
        assert_eq!(out, [(0x1004, 0x100c), (0x1014, 0x1018), (0x107c, 0x1080)]);
        out.clear();
        page.runs_in(0x2000, 0x3000, &mut out);
        assert!(out.is_empty());
    }

    /// What a landing skips: masks not taken, and masks taken by sends after
    /// the fetch; those of sends a merged fetch came after are let go.
    #[test]
    fn a_landing_skips_what_may_not_be_on_the_card_yet() {
        let one = |at: usize| {
            let mut mask = [0u32; WORDS / 32];
            mask[0] = 1;
            Masked { at, words: 1, mask }
        };
        let mut masks = Masks::default();
        masks.put(vec![one(0)]);
        let near = |masks: &Masks, after: u64| {
            let mut out = Vec::new();
            masks.near(after, 0, 1 << 20, &mut out);
            out
        };
        assert_eq!(near(&masks, 0), [(0, 4)], "not taken: skipped");
        assert_eq!(masks.take(5, true, 0), [one(0)]);
        assert!(masks.dirty.is_empty());
        assert_eq!(near(&masks, 4), [(0, 4)], "taken by a send after the fetch");
        assert!(near(&masks, 5).is_empty(), "the fetch ran behind the send");
        masks.put(vec![one(8)]);
        assert_eq!(masks.take(6, true, 5), [one(8)]);
        assert_eq!(near(&masks, 4), [(8, 12)], "the send up to 5 is let go");
        assert!(masks.take(7, false, 0).is_empty());
    }
}

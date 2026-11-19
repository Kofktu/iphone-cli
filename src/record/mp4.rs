//! A one-track HEVC MP4, written as the frames come: the samples go into
//! `<file>.part` straight away and only the sample table stays in memory.
//! `finish` appends the index (`moov`) and renames the file into place, so
//! a file without `.part` is always complete.

use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use super::hevc::{ParameterSets, RTP_CLOCK};

/// One frame's entry in the sample table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub size: u32,
    /// In RTP clock ticks.
    pub duration: u32,
    pub key: bool,
}

/// Ticks from one RTP timestamp to a later one, across the 32-bit wrap. A
/// timestamp that went backwards gives one tick, so time never runs back.
pub fn ticks_between(earlier: u32, later: u32) -> u32 {
    match later.wrapping_sub(earlier) {
        0 => 1,
        d if d > u32::MAX / 2 => 1,
        d => d,
    }
}

/// Microseconds of wall clock as RTP ticks, at least one.
pub fn ticks_from_micros(us: u64) -> u32 {
    (us.saturating_mul(u64::from(RTP_CLOCK)) / 1_000_000).clamp(1, u64::from(u32::MAX)) as u32
}

pub fn seconds(ticks: u64) -> f64 {
    ticks as f64 / f64::from(RTP_CLOCK)
}

const FTYP: &[u8] = b"isom\0\0\x02\0isomiso2mp41";
/// `mdat` with a 64-bit size, filled in by `finish`.
const MDAT_HEADER: usize = 16;

pub struct Writer {
    path: PathBuf,
    part: PathBuf,
    file: BufWriter<File>,
    samples: Vec<Sample>,
    data_bytes: u64,
    total_ticks: u64,
}

impl Writer {
    pub fn part_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".part");
        PathBuf::from(name)
    }

    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let part = Self::part_path(path);
        let mut file = BufWriter::new(File::create(&part)?);
        file.write_all(&boxed(b"ftyp", FTYP))?;
        file.write_all(&1u32.to_be_bytes())?;
        file.write_all(b"mdat")?;
        file.write_all(&0u64.to_be_bytes())?;
        Ok(Self { path: path.to_path_buf(), part, file, samples: Vec::new(), data_bytes: 0, total_ticks: 0 })
    }

    /// Append a frame's data. Its duration is set when the next one comes.
    pub fn append(&mut self, data: &[u8], key: bool) -> std::io::Result<()> {
        self.file.write_all(data)?;
        self.data_bytes += data.len() as u64;
        self.samples.push(Sample { size: data.len() as u32, duration: 0, key });
        Ok(())
    }

    /// Set the duration of the last frame appended.
    pub fn set_last_duration(&mut self, ticks: u32) {
        if let Some(s) = self.samples.last_mut() {
            self.total_ticks -= u64::from(s.duration);
            s.duration = ticks.max(1);
            self.total_ticks += u64::from(s.duration);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    pub fn total_ticks(&self) -> u64 {
        self.total_ticks
    }

    /// Write the index and move the file into place. Returns its size.
    pub fn finish(mut self, params: &ParameterSets) -> std::io::Result<u64> {
        let moov = moov(&self.samples, params, (FTYP.len() + 8 + MDAT_HEADER) as u32);
        self.file.write_all(&moov)?;
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start((FTYP.len() + 8 + 8) as u64))?;
        file.write_all(&(MDAT_HEADER as u64 + self.data_bytes).to_be_bytes())?;
        file.sync_all()?;
        let size = file.metadata()?.len();
        drop(file);
        std::fs::rename(&self.part, &self.path)?;
        Ok(size)
    }

    /// Give up: remove the partial file.
    pub fn discard(self) {
        let part = self.part.clone();
        drop(self);
        let _ = std::fs::remove_file(part);
    }
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], version_flags: u32, body: &[u8]) -> Vec<u8> {
    boxed(kind, &[&version_flags.to_be_bytes()[..], body].concat())
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// The index for samples stored back to back in one chunk at `chunk_offset`.
fn moov(samples: &[Sample], params: &ParameterSets, chunk_offset: u32) -> Vec<u8> {
    let timescale = RTP_CLOCK;
    let total = samples.iter().map(|s| u64::from(s.duration)).sum::<u64>().min(u64::from(u32::MAX)) as u32;
    let (w, h) = (params.info.width, params.info.height);

    let mut mvhd = words(&[0, 0, timescale, total, 0x10000]);
    mvhd.extend_from_slice(&[1, 0, 0, 0]);
    mvhd.extend_from_slice(&[0; 8]);
    mvhd.extend_from_slice(&words(&MATRIX));
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());
    let mvhd = full(b"mvhd", 0, &mvhd);

    // Enabled, in movie, in preview.
    let mut tkhd = words(&[0, 0, 1, 0, total, 0, 0]);
    tkhd.extend_from_slice(&[0; 8]);
    tkhd.extend_from_slice(&words(&MATRIX));
    tkhd.extend_from_slice(&words(&[w << 16, h << 16]));
    let tkhd = full(b"tkhd", 3, &tkhd);

    let mut mdhd = words(&[0, 0, timescale, total]);
    // Language "und".
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]);
    let mdhd = full(b"mdhd", 0, &mdhd);
    let hdlr = full(b"hdlr", 0, b"\0\0\0\0vide\0\0\0\0\0\0\0\0\0\0\0\0VideoHandler\0");

    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0; 16]);
    entry.extend_from_slice(&(w as u16).to_be_bytes());
    entry.extend_from_slice(&(h as u16).to_be_bytes());
    // 72 dpi both ways, no data size, one frame per sample.
    entry.extend_from_slice(&words(&[0x0048_0000, 0x0048_0000, 0]));
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0; 32]);
    entry.extend_from_slice(&0x0018u16.to_be_bytes());
    entry.extend_from_slice(&0xffffu16.to_be_bytes());
    entry.extend_from_slice(&boxed(b"hvcC", &params.hvcc()));
    let stsd = full(b"stsd", 0, &[&1u32.to_be_bytes()[..], &boxed(b"hvc1", &entry)].concat());

    let mut runs: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        match runs.last_mut() {
            Some((count, duration)) if *duration == s.duration => *count += 1,
            _ => runs.push((1, s.duration)),
        }
    }
    let mut stts = words(&[runs.len() as u32]);
    for (count, duration) in &runs {
        stts.extend_from_slice(&words(&[*count, *duration]));
    }
    let stts = full(b"stts", 0, &stts);

    let keys: Vec<u32> = samples.iter().zip(1..).filter(|(s, _)| s.key).map(|(_, n)| n).collect();
    let stss = full(b"stss", 0, &[words(&[keys.len() as u32]), words(&keys)].concat());

    let sizes: Vec<u32> = samples.iter().map(|s| s.size).collect();
    let stsz = full(b"stsz", 0, &[words(&[0, samples.len() as u32]), words(&sizes)].concat());
    let stsc = full(b"stsc", 0, &words(&[1, 1, samples.len() as u32, 1]));
    let stco = full(b"stco", 0, &words(&[1, chunk_offset]));

    let stbl = boxed(b"stbl", &[stsd, stts, stss, stsz, stsc, stco].concat());
    let vmhd = full(b"vmhd", 1, &[0; 8]);
    let dinf = boxed(b"dinf", &full(b"dref", 0, &[&1u32.to_be_bytes()[..], &full(b"url ", 1, &[])].concat()));
    let minf = boxed(b"minf", &[vmhd, dinf, stbl].concat());
    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = boxed(b"trak", &[tkhd, mdia].concat());
    boxed(b"moov", &[mvhd, trak].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::hevc::{AccessUnit, NAL_PPS, NAL_VPS, Sps, tests::sps_nal};

    /// Every box at this level: (type, body).
    fn children(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 8 <= data.len() {
            let mut size = u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = data[off + 4..off + 8].try_into().unwrap();
            let mut header = 8;
            if size == 1 {
                size = u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize;
                header = 16;
            }
            out.push((kind, &data[off + header..off + size]));
            off += size;
        }
        out
    }

    fn find<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
        let (first, rest) = path.split_first().unwrap();
        let body = children(data).into_iter().find(|(k, _)| k == *first).unwrap_or_else(|| panic!("no {:?}", std::str::from_utf8(*first))).1;
        if rest.is_empty() { body } else { find(body, rest) }
    }

    fn word(b: &[u8], i: usize) -> u32 {
        u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
    }

    fn params(w: u32, h: u32) -> ParameterSets {
        let au = AccessUnit {
            nals: vec![vec![NAL_VPS << 1, 1, 0xaa], sps_nal(w, h), vec![NAL_PPS << 1, 1, 0xbb], vec![19 << 1, 1, 5]],
            ..Default::default()
        };
        ParameterSets::from_keyframe(&au).unwrap()
    }

    #[test]
    fn rtp_time_becomes_media_time_at_24_khz() {
        assert_eq!(ticks_between(1000, 1400), 400);
        assert_eq!(ticks_between(u32::MAX - 99, 300), 400, "across the wrap");
        assert_eq!(ticks_between(1400, 1000), 1, "never backwards");
        assert_eq!(ticks_between(7, 7), 1);
        assert_eq!(ticks_from_micros(500_000), 12_000);
        assert_eq!(ticks_from_micros(0), 1);
        assert!((seconds(240_000) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn the_file_parses_back_with_its_samples_durations_and_decoder_setup() {
        let dir = std::env::temp_dir().join(format!("iph-mp4-{}", std::process::id()));
        let path = dir.join("clip.mp4");
        let p = params(1408, 2016);
        let mut w = Writer::create(&path).unwrap();
        assert!(Writer::part_path(&path).exists() && !path.exists());
        let frames: [(&[u8], bool, u32); 4] = [(&[1; 10], true, 400), (&[2; 3], false, 400), (&[3; 7], false, 800), (&[4; 5], false, 400)];
        for (data, key, duration) in frames {
            w.append(data, key).unwrap();
            w.set_last_duration(duration);
        }
        assert_eq!(w.total_ticks(), 2000);
        let size = w.finish(&p).unwrap();
        let file = std::fs::read(&path).unwrap();
        assert_eq!(size, file.len() as u64);
        assert!(!Writer::part_path(&path).exists());

        let top: Vec<[u8; 4]> = children(&file).iter().map(|(k, _)| *k).collect();
        assert_eq!(top, [*b"ftyp", *b"mdat", *b"moov"]);
        assert_eq!(find(&file, &[b"mdat"]), [&[1u8; 10][..], &[2; 3], &[3; 7], &[4; 5]].concat());

        let mvhd = find(&file, &[b"moov", b"mvhd"]);
        assert_eq!((word(mvhd, 3), word(mvhd, 4)), (RTP_CLOCK, 2000));
        let stbl = find(&file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
        let stts = find(stbl, &[b"stts"]);
        assert_eq!(&stts[4..], words(&[3, 2, 400, 1, 800, 1, 400]).as_slice());
        let stsz = find(stbl, &[b"stsz"]);
        assert_eq!(&stsz[4..], words(&[0, 4, 10, 3, 7, 5]).as_slice());
        assert_eq!(&find(stbl, &[b"stss"])[4..], words(&[1, 1]).as_slice());
        let stco = find(stbl, &[b"stco"]);
        let offset = word(stco, 2) as usize;
        assert_eq!(&file[offset..offset + 10], &[1; 10]);

        let stsd = find(stbl, &[b"stsd"]);
        let hvc1 = find(&stsd[8..], &[b"hvc1"]);
        assert_eq!((u16::from_be_bytes([hvc1[24], hvc1[25]]), u16::from_be_bytes([hvc1[26], hvc1[27]])), (1408, 2016));
        let hvcc = find(&hvc1[78..], &[b"hvcC"]);
        assert_eq!(hvcc[0], 1);
        assert_eq!(hvcc[21] & 3, 3, "4-byte NAL lengths");
        assert_eq!(hvcc[22], 3, "VPS, SPS and PPS");
        let sps_at = 23 + 5 + 3;
        let sps_len = u16::from_be_bytes([hvcc[sps_at + 3], hvcc[sps_at + 4]]) as usize;
        let sps = &hvcc[sps_at + 5..sps_at + 5 + sps_len];
        assert_eq!(Sps::parse(sps).map(|s| (s.width, s.height)), Some((1408, 2016)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_discarded_recording_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("iph-mp4-discard-{}", std::process::id()));
        let path = dir.join("clip.mp4");
        let w = Writer::create(&path).unwrap();
        w.discard();
        assert!(!Writer::part_path(&path).exists() && !path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

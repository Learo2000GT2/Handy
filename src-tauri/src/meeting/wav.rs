//! The meeting's audio file: 16 kHz mono 16-bit PCM WAV, written while the
//! meeting is recorded. Every checkpoint updates the header and forces the
//! data to disk, so a crash or power loss loses at most the audio since the
//! last checkpoint. [`repair`] fixes the header of a file whose writer died
//! between checkpoints, so every byte that reached the disk is kept.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const SAMPLE_RATE: u32 = 16_000;
const BYTES_PER_SAMPLE: u64 = 2;

pub struct DurableWav {
    writer: hound::WavWriter<BufWriter<File>>,
    /// Second handle to the same file, for `sync_data` (hound has no way to
    /// reach its own).
    file: File,
    samples: u64,
}

fn to_io(error: hound::Error) -> io::Error {
    match error {
        hound::Error::IoError(error) => error,
        other => io::Error::other(other),
    }
}

/// Converts one sample to 16-bit PCM, clamping out-of-range values.
fn to_pcm(sample: f32) -> i16 {
    if sample.is_nan() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

impl DurableWav {
    /// Creates the file (failing if it exists) and makes its header durable.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(path)?;
        let sync_handle = file.try_clone()?;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::new(BufWriter::new(file), spec).map_err(to_io)?;
        let mut wav = Self {
            writer,
            file: sync_handle,
            samples: 0,
        };
        wav.checkpoint()?;
        Ok(wav)
    }

    /// Appends 16 kHz mono samples. They are durable after the next
    /// [`checkpoint`](Self::checkpoint).
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        for &sample in samples {
            self.writer.write_sample(to_pcm(sample)).map_err(to_io)?;
        }
        self.samples += samples.len() as u64;
        Ok(())
    }

    /// Updates the header and forces everything written so far to disk.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.writer.flush().map_err(to_io)?;
        self.file.sync_data()
    }

    /// Finishes the file and forces it to disk. Returns the sample count.
    pub fn finish(self) -> io::Result<u64> {
        let samples = self.samples;
        self.writer.finalize().map_err(to_io)?;
        self.file.sync_all()?;
        Ok(samples)
    }
}

impl DurableWav {
    /// Writes to an existing file or device (tests: `/dev/full`).
    #[cfg(test)]
    pub fn open_existing(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).read(true).open(path)?;
        let sync_handle = file.try_clone()?;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::new(BufWriter::new(file), spec).map_err(to_io)?;
        Ok(Self {
            writer,
            file: sync_handle,
            samples: 0,
        })
    }

    /// Gives up on the file after a write error and makes what reached the
    /// disk readable. The writer is leaked, not dropped: after a failed
    /// checkpoint hound's position can be left inside the header, and its
    /// drop would write there, over the start of the audio.
    pub fn abandon(self, path: &Path) -> io::Result<u64> {
        std::mem::forget(self);
        repair(path)
    }
}

/// Milliseconds of audio in `samples` samples.
pub fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / u64::from(SAMPLE_RATE)
}

/// Makes the RIFF and data sizes in the header match the audio actually in
/// the file, dropping a trailing partial sample. Returns the number of
/// samples. Safe on a file that is already correct.
pub fn repair(path: &Path) -> io::Result<u64> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();

    let mut riff = [0u8; 12];
    file.read_exact(&mut riff)
        .map_err(|_| invalid("the file is too short to be a WAV file"))?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(invalid("the file is not a WAV file"));
    }

    // Walk the chunks to the data chunk; its size field is the one to fix.
    let mut pos = 12u64;
    let data_start = loop {
        if pos + 8 > len {
            return Err(invalid("the WAV file has no data chunk"));
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let size = u64::from(u32::from_le_bytes([
            header[4], header[5], header[6], header[7],
        ]));
        if &header[0..4] == b"data" {
            break pos + 8;
        }
        pos += 8 + size + (size & 1);
    };

    let data_len = (len - data_start) / BYTES_PER_SAMPLE * BYTES_PER_SAMPLE;
    let data_len = data_len.min(u64::from(u32::MAX) - (data_start - 8));
    let end = data_start + data_len;
    if end < len {
        file.set_len(end)?;
    }
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&((end - 8) as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(data_start - 4))?;
    file.write_all(&(data_len as u32).to_le_bytes())?;
    file.sync_all()?;
    Ok(data_len / BYTES_PER_SAMPLE)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize) -> Vec<f32> {
        (0..len).map(|i| ((i as f32) * 0.05).sin() * 0.5).collect()
    }

    fn read_back(path: &Path) -> Vec<i16> {
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn finished_file_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        let mut wav = DurableWav::create(&path).unwrap();
        wav.write(&tone(1600)).unwrap();
        wav.checkpoint().unwrap();
        wav.write(&tone(800)).unwrap();
        assert_eq!(wav.finish().unwrap(), 2400);

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_rate, SAMPLE_RATE);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(read_back(&path).len(), 2400);
    }

    #[test]
    fn create_refuses_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        std::fs::write(&path, b"keep me").unwrap();
        assert!(DurableWav::create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
    }

    #[test]
    fn checkpoint_leaves_a_readable_file_if_the_writer_dies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        let mut wav = DurableWav::create(&path).unwrap();
        wav.write(&tone(16_000)).unwrap();
        wav.checkpoint().unwrap();
        // A crash: the writer is never finalized or dropped normally.
        std::mem::forget(wav);

        assert_eq!(read_back(&path).len(), 16_000);
    }

    #[test]
    fn repair_recovers_audio_written_after_the_last_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        let mut wav = DurableWav::create(&path).unwrap();
        wav.write(&tone(16_000)).unwrap();
        wav.checkpoint().unwrap();
        std::mem::forget(wav);

        // Bytes that reached the disk after the checkpoint, plus half a
        // sample cut off by the power loss.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[1, 0].repeat(4000)).unwrap();
        file.write_all(&[7]).unwrap();
        drop(file);
        assert_eq!(read_back(&path).len(), 16_000);

        assert_eq!(repair(&path).unwrap(), 20_000);
        assert_eq!(read_back(&path).len(), 20_000);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 44 + 40_000);
        // Repairing again changes nothing.
        assert_eq!(repair(&path).unwrap(), 20_000);
    }

    #[test]
    fn repair_fixes_a_header_that_was_never_updated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        let wav = DurableWav::create(&path).unwrap();
        std::mem::forget(wav);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0, 1].repeat(320)).unwrap();
        drop(file);

        assert_eq!(repair(&path).unwrap(), 320);
        assert_eq!(read_back(&path).len(), 320);
    }

    #[test]
    fn repair_rejects_files_that_are_not_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        assert!(repair(&path).is_err());
        std::fs::write(&path, b"RIFF\0\0\0\0AVI LIST\0\0\0\0").unwrap();
        assert!(repair(&path).is_err());
    }

    #[test]
    fn abandon_keeps_what_was_checkpointed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meeting.wav");
        let mut wav = DurableWav::create(&path).unwrap();
        wav.write(&tone(16_000)).unwrap();
        wav.checkpoint().unwrap();
        assert_eq!(wav.abandon(&path).unwrap(), 16_000);
        assert_eq!(read_back(&path).len(), 16_000);
    }

    #[test]
    fn samples_are_clamped_to_16_bit() {
        assert_eq!(to_pcm(2.0), i16::MAX);
        assert_eq!(to_pcm(-2.0), -i16::MAX);
        assert_eq!(to_pcm(f32::NAN), 0);
        assert_eq!(to_pcm(0.0), 0);
    }
}

#![warn(rust_2018_idioms)]

use std::io;
use std::io::{Seek, SeekFrom};

use sonata_core::audio::{AudioBuffer, SignalSpec, Timestamp};
use sonata_core::codecs::{CODEC_TYPE_WAVE, CodecParameters, DecoderOptions};
use sonata_core::errors::{Result, Error, decode_error, seek_error, unsupported_error, SeekErrorKind};
use sonata_core::formats::{Packet, Stream, SeekIndex};
use sonata_core::io::*;
use sonata_core::tags::{Tag, RiffTag};

pub use sonata_core::formats::{ProbeDepth, ProbeResult, Format, FormatReader, SeekSearchResult};
pub use sonata_core::codecs::Decoder;

/// The recommended maximum number of bytes advance a stream to find the stream marker before giving up.
const WAVE_PROBE_SEARCH_LIMIT: usize = 512 * 1024;

use std::marker::PhantomData;

trait MakeParser : Sized {
    fn make_parser(tag: &[u8; 4], len: u32) -> Option<Self>;
}

struct ChunkReader<T: MakeParser> {
    len: u32,
    consumed: u32,
    phantom: PhantomData<T>,
}

impl<T: MakeParser> ChunkReader<T> {
    fn new(len: u32) -> Self {
        ChunkReader { 
            len, 
            consumed: 0, 
            phantom: PhantomData
        }
    }

    fn next<B: Bytestream>(&mut self, reader: &mut B) -> Result<Option<T>> {
        // Loop until a chunk is recognized and returned, or the end of stream is reached.
        loop {
            // Align to the next 2-byte boundary if not currently aligned..
            if self.consumed & 0x1 == 1 {
                reader.read_u8()?;
                self.consumed += 1;
            }

            // Check if there are enough bytes for another chunk, if not, there are no more chunks.
            if self.consumed + 8 > self.len {
                return Ok(None);
            }

            // Read tag and len, the chunk header.
            let tag = reader.read_quad_bytes()?;
            let len = reader.read_u32()?;
            self.consumed += 8;

            // Check if the chunk length will exceed the parent chunk.
            if self.consumed + len > self.len {
                return decode_error("Info chunk length exceeds parent List chunk length.");
            }

            // "Consume" the chunk.
            self.consumed += len;

            match T::make_parser(&tag, len) {
                Some(chunk) => return Ok(Some(chunk)),
                None => {
                    // As per the RIFF spec, unknown chunks are to be ignored.
                    eprintln!("Ignoring unknown chunk: tag={}, len={}.", String::from_utf8_lossy(&tag), len);
                    reader.ignore_bytes(len as u64)?
                }
            }
        }
    }

    fn finish<B: Bytestream>(&mut self, reader: &mut B) -> Result<()>{
        // If data is remaining in this chunk, skip it.
        if self.consumed < self.len {
            let remaining = self.len - self.consumed;
            reader.ignore_bytes(remaining as u64)?;
            self.consumed += remaining;
        }

        // Pad the chunk to the next 2-byte boundary.
        if self.len & 0x1 == 1 {
            reader.read_u8()?;
        }

        Ok(())
    }
}

trait ParseChunk : Sized {
    fn parse<B: Bytestream>(reader: &mut B, tag: [u8; 4], len: u32) -> Result<Self>;
}

struct ChunkParser<P: ParseChunk> {
    tag: [u8; 4],
    len: u32,
    phantom: PhantomData<P>,
}

impl<P: ParseChunk> ChunkParser<P> {
    fn new(tag: [u8; 4], len: u32) -> Self {
        ChunkParser {
            tag,
            len,
            phantom: PhantomData,
        }
    }

    fn parse<B: Bytestream>(&self, reader: &mut B) -> Result<P> {
        P::parse(reader, self.tag, self.len)
    }
}







enum WaveFormatData {
    Pcm(WaveFormatPcm),
    IeeeFloat,
    Extensible(WaveFormatExtensible),
}

struct WaveFormatPcm {
    /// The number of bits per sample. In the PCM format, this is always a multiple of 8-bits.
    bits_per_sample: u16,
}

struct WaveFormatExtensible {
    /// The number of bits per sample rounded up to the nearest 8-bits.
    bits_per_sample: u16,
    /// The number of bits per sample.
    bits_per_coded_sample: u16,
    /// Mask of channels.
    channel_mask: u32,
    /// Globally unique identifier of the format.
    sub_format_guid: [u8; 16],
}

struct WaveFormatChunk {
    /// The number of channels.
    n_channels: u16,
    /// The sample rate in Hz. For non-PCM formats, this value must be interpreted as per the format's specifications.
    sample_rate: u32,
    /// The required average data rate required in bytes/second. For non-PCM formats, this value must be interpreted as 
    /// per the format's specifications.
    avg_bytes_per_sec: u32,
    /// The byte alignment of one audio frame. For PCM formats, this is equal to 
    /// `(n_channels * extra_data.bits_per_sample) / 8`. For non-PCM formats, this value must be interpreted as per the 
    /// format's specifications.
    block_align: u16,
    /// Extra data associated with the format block conditional upon the format tag.
    format_data: WaveFormatData,
}

impl WaveFormatChunk {

    fn read_pcm_fmt<B: Bytestream>(reader: &mut B, bits_per_sample: u16, chunk_len: u32) -> Result<WaveFormatData> {
        // WaveFormat for a PCM format /may/ be extended with an extra data length parameter followed by the 
        // extra data itself. Use the chunk length to determine if the format chunk is extended.
        let is_extended = match chunk_len {
            // Minimal WavFormat struct, no extension.
            16 => false,
            // WaveFormatEx with exta data length field present, but not extra data.
            18 => true,
            // WaveFormatEx with extra data length field and extra data.
            40 => true,
            _ => return decode_error("Malformed PCM fmt chunk."),
        };

        // If there is extra data, read the length, and discard the extra data.
        if is_extended {
            let extra_size = reader.read_u16()?; 

            if extra_size > 0 {
                reader.ignore_bytes(extra_size as u64)?;
            }
        }

        // Bits per sample for PCM is both the decoded width, and actual sample width. Strictly, this must 
        // either be 8 or 16 bits, but there is no reason why 24 and 32 bits can't be supported. Since these 
        // files do exist, allow 8/16/24/32-bit, but error if not a multiple of 8 or greater than 32-bits.
        if (bits_per_sample > 32) || (bits_per_sample & 0x7 != 0) {
            return decode_error("Bits per sample for PCM Wave Format must either be 8 or 16 bits.");
        }

        Ok(WaveFormatData::Pcm(WaveFormatPcm { bits_per_sample }))
    }

    fn read_ieee_fmt<B: Bytestream>(reader: &mut B, bits_per_sample: u16, chunk_len: u32) -> Result<WaveFormatData> {
        // WaveFormat for a IEEE format should not be extended, but it may still have an extra data length 
        // parameter.
        if chunk_len == 18 {
            let extra_size = reader.read_u16()?; 
            if extra_size != 0 {
                return decode_error("Extra data not expected for IEEE fmt chunk.");
            }
        }
        else if chunk_len > 16 {
            return decode_error("Malformed IEEE fmt chunk.");
        }

        // Officially, only 32-bit floats are supported, but Sonata can handle 64-bit floats.
        if bits_per_sample != 32 || bits_per_sample != 64 {
            return decode_error("Bits per sample for IEEE Wave Format must be 32-bits.");
        }

        Ok(WaveFormatData::IeeeFloat)
    }

    fn read_ext_fmt<B: Bytestream>(reader: &mut B, bits_per_sample: u16, chunk_len: u32) -> Result<WaveFormatData> {
        // WaveFormat for the extensible format must be extended to 40 bytes in length.
        if chunk_len < 40 {
            return decode_error("Malformed Extensible fmt chunk.");
        }

        let extra_size = reader.read_u16()?; 

        // The size of the extra data for the Extensible format is exactly 22 bytes.
        if extra_size != 22 {
            return decode_error("Extra data size not 22 bytes for Extensible fmt chunk.");
        }

        // Bits per sample for extensible formats is the decoded "container" width per sample. This must be 
        // a multiple of 8.
        if bits_per_sample % 8 > 0 {
            return decode_error("Bits per sample for Extensible Wave Format must be a multiple of 8 bits.");
        }
        
        let bits_per_coded_sample = reader.read_u16()?;
        let channel_mask = reader.read_u32()?;
        let mut sub_format_guid = [0u8; 16];

        reader.read_buf_bytes(&mut sub_format_guid)?;

        // These GUIDs identifiy the format of the data chunks. These definitions can be found in ksmedia.h of the 
        // Microsoft Windows Platform SDK.
        const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] = 
            [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
        // const KSDATAFORMAT_SUBTYPE_ADPCM: [u8; 16] = 
        //     [0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x10, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
        const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = 
            [0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
        // const KSDATAFORMAT_SUBTYPE_ALAW: [u8; 16] = 
        //     [0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x10, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
        // const KSDATAFORMAT_SUBTYPE_MULAW: [u8; 16] = 
        //     [0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x10, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];

        // Verify support based on the format GUID.
        match sub_format_guid {
            KSDATAFORMAT_SUBTYPE_PCM => {}
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT => {},
            _ => return unsupported_error("Unsupported Wave Format."),
        };

        Ok(WaveFormatData::Extensible(WaveFormatExtensible { 
            bits_per_sample, bits_per_coded_sample, channel_mask, sub_format_guid }))
    }
}

impl ParseChunk for WaveFormatChunk {
    fn parse<B: Bytestream>(reader: &mut B, _tag: [u8; 4], len: u32) -> Result<WaveFormatChunk> {
        let format = reader.read_u16()?;
        let n_channels = reader.read_u16()?;
        let sample_rate = reader.read_u32()?;
        let avg_bytes_per_sec = reader.read_u32()?;
        let block_align = reader.read_u16()?;
        let bits_per_sample = reader.read_u16()?;

        // The definition of these format identifiers can be found in mmreg.h of the Microsoft Windows Platform SDK.
        const WAVE_FORMAT_PCM: u16        = 0x0001;
        // const WAVE_FORMAT_ADPCM: u16        = 0x0002;
        const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
        // const WAVE_FORMAT_ALAW: u16       = 0x0006;
        // const WAVE_FORMAT_MULAW: u16      = 0x0007;
        const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

        let format_data = match format {
            // The PCM Wave Format
            WAVE_FORMAT_PCM => Self::read_pcm_fmt(reader, bits_per_sample, len),
            // The IEEE Float Wave Format
            WAVE_FORMAT_IEEE_FLOAT => Self::read_ieee_fmt(reader, bits_per_sample, len),
            // The Extensible Wave Format
            WAVE_FORMAT_EXTENSIBLE => Self::read_ext_fmt(reader, bits_per_sample, len),
            // Unsupported format.
            _ => unsupported_error("Unsupported Wave Format."),
        }?;

        Ok(WaveFormatChunk { n_channels, sample_rate, avg_bytes_per_sec, block_align, format_data })
    }
}


struct FactChunk {
    n_frames: u32,
}

impl ParseChunk for FactChunk {
    fn parse<B: Bytestream>(reader: &mut B, _tag: [u8; 4], _len: u32) -> Result<Self> {
        Ok(FactChunk{ n_frames: reader.read_u32()? })
    }
}

struct ListChunk {
    form: [u8; 4],
    len: u32, 
}

impl ListChunk {
    fn form(&self) -> [u8; 4] {
        self.form
    }

    fn len(&self) -> u32 {
        self.len
    }

    fn skip<B: Bytestream>(&self, reader: &mut B) -> Result<()> {
        unimplemented!("Skipping list chunks is unimplemented.")
    }
}

impl ParseChunk for ListChunk {
    fn parse<B: Bytestream>(reader: &mut B, _tag: [u8; 4], len: u32) -> Result<Self> {
        Ok(ListChunk{ 
            form: reader.read_quad_bytes()?,
            len: len - 4
        })
    }
}

struct InfoChunk {
    tag: Tag,
}

impl ParseChunk for InfoChunk {
    fn parse<B: Bytestream>(reader: &mut B, tag: [u8; 4], len: u32) -> Result<InfoChunk> {
        let mut value_buf = vec![0u8; len as usize];
        reader.read_buf_bytes(&mut value_buf)?;

        let value = String::from_utf8_lossy(&value_buf);

        Ok(InfoChunk {
            tag: RiffTag::parse(tag, &value)
        })
    }
}

enum RiffWaveChunks {
    Format(ChunkParser<WaveFormatChunk>),
    List(ChunkParser<ListChunk>),
    Fact(ChunkParser<FactChunk>),
    Data
}

impl MakeParser for RiffWaveChunks {
    fn make_parser(tag: &[u8; 4], len: u32) -> Option<Self> {
        match tag {
            b"fmt " => Some(RiffWaveChunks::Format(ChunkParser::<WaveFormatChunk>::new(*tag, len))),
            b"LIST" => Some(RiffWaveChunks::List(ChunkParser::<ListChunk>::new(*tag, len))),
            b"fact" => Some(RiffWaveChunks::Fact(ChunkParser::<FactChunk>::new(*tag, len))),
            b"data" => Some(RiffWaveChunks::Data),
            _ => None,
        }
    }
}

enum RiffInfoListChunks {
    Info(ChunkParser<InfoChunk>),
}

impl MakeParser for RiffInfoListChunks {
    fn make_parser(tag: &[u8; 4], len: u32) -> Option<Self> {
        // Right now it is assumed all list chunks are INFO chunks, but that's not really guaranteed.
        // TODO: Actually validate that the chunk is an info chunk.
        Some(RiffInfoListChunks::Info(ChunkParser::<InfoChunk>::new(*tag, len)))
    }
}


/// `Wav` (Wave) is the Free Lossless Audio Codec.
/// 
/// This format only supports reading.
pub struct Wav;

impl Format for Wav {
    type Reader = WavReader;

    fn open<S: 'static + MediaSource>(source: Box<S>) -> Self::Reader {
        let mss = MediaSourceStream::new(source);
        WavReader::open(mss)
    }
}

/// `WavReader` implements a demultiplexer for the native Wav format container.
pub struct WavReader {
    reader: MediaSourceStream,
    streams: Vec<Stream>,
    index: Option<SeekIndex>,
}

impl WavReader {

    pub fn open(source: MediaSourceStream) -> Self {
        WavReader {
            reader: source,
            streams: Vec::new(),
            index: None,
        }
    }

    fn read_metadata(&mut self, len: u32) -> Result<()> {
        let mut info_list = ChunkReader::<RiffInfoListChunks>::new(len);

        loop {
            let chunk = info_list.next(&mut self.reader)?;

            if chunk.is_none() {
                break;
            }

            match chunk.unwrap() {
                RiffInfoListChunks::Info(nfo) => { 
                    let info = nfo.parse(&mut self.reader)?;
                    eprintln!("{}", info.tag); 
                }
            }
        }
        
        info_list.finish(&mut self.reader)?;

        Ok(())
    }

}

impl FormatReader for WavReader {

    fn next_packet(&mut self) -> Result<Packet<'_, MediaSourceStream>> {
        // Return next RIFF chunk.
        unsupported_error("Packet streaming is unsupported")
    }

    fn streams(&self) -> &[Stream] {
        &self.streams
    }

    fn seek(&mut self, ts: Timestamp) -> Result<u64> {
        unsupported_error("Seeking is unsupported")
    }

    fn probe(&mut self, depth: ProbeDepth) -> Result<ProbeResult> {

        // Search for the "RIFF" marker.
        let marker = search_for_marker(&mut self.reader, b"RIFF", depth)?;

        if marker.is_none() {
            return Ok(ProbeResult::Unsupported);
        }

        // A Wave file is one large RIFF chunk, with the actual meta and audio data as sub-chunks. Therefore, 
        // the header was the chunk ID, and the next 4 bytes is the length of the RIFF chunk.
        let riff_len = self.reader.read_u32()?;
        let riff_form = self.reader.read_quad_bytes()?;

        // The RIFF chunk contains WAVE data.
        if riff_form != *b"wave" {

            let mut riff_chunks = ChunkReader::<RiffWaveChunks>::new(riff_len);
            
            loop {
                let chunk = riff_chunks.next(&mut self.reader)?;

                // The last chunk should always be a data chunk. Probe will exit with a supported result in that case.
                // Therefore, if there is no more chunks left, then the file is unsupported. Exit.
                if chunk.is_none() {
                    break;
                }

                match chunk.unwrap() {
                    RiffWaveChunks::Format(fmt) => {
                        let format = fmt.parse(&mut self.reader)?;
                    },
                    RiffWaveChunks::Fact(fct) => {
                        let fact = fct.parse(&mut self.reader)?;
                    },
                    RiffWaveChunks::List(lst) => {
                        let list = lst.parse(&mut self.reader)?;

                        // Riff Lists can have many different forms, but WavReader only supports Info lists.
                        match &list.form() {
                            b"INFO" => self.read_metadata(list.len())?,
                            _ => list.skip(&mut self.reader)?
                        }
                    },
                    RiffWaveChunks::Data => {
                        return Ok(ProbeResult::Supported);
                    }
                }
            }
        }

        // Not supported.
        Ok(ProbeResult::Unsupported)
    }

}

fn search_for_marker<B: Bytestream>(reader: &mut B, marker: &[u8; 4], depth: ProbeDepth) -> Result<Option<[u8; 4]>> {
    let mut window = [0u8; 4];

    reader.read_buf_bytes(&mut window)?;

    // Count the number of bytes read in the probe so that a limit may (optionally) be applied.
    let mut probed_bytes = 4usize;

    loop {
        if window == *marker {
            // Found the marker.
            eprintln!("Probe: Found stream marker @ +{} bytes.", probed_bytes - 4);
            return Ok(Some(*marker));
        }
        // If the ProbeDepth is deep, continue searching for the stream marker.
        else if depth == ProbeDepth::Deep {
            // Do not search more than the designated search limit.
            if probed_bytes <= WAVE_PROBE_SEARCH_LIMIT {

                if probed_bytes % 4096 == 0 {
                    eprintln!("Probe: Searching for stream marker... ({} / {}) bytes.", 
                        probed_bytes, WAVE_PROBE_SEARCH_LIMIT);
                }

                window[0] = window[1];
                window[1] = window[2];
                window[2] = window[3];
                window[3] = reader.read_u8()?;

                probed_bytes += 1;
            }
            else {
                eprintln!("Probe: Stream marker search limit exceeded.");
                break;
            }
        }
        else {
            break;
        }
    }

    // Loop exited, therefore stream is unsupported.
    Ok(None)
}

/// `WavDecoder` implements a decoder for the Wav codec bitstream. The decoder is compatible with OGG encapsulated 
/// Wav.
pub struct WavDecoder {
    params: CodecParameters,
}

impl Decoder for WavDecoder {

    fn new(params: &CodecParameters, options: &DecoderOptions) -> Self {
        WavDecoder {
            params: params.clone(),
        }
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn spec(&self) -> Option<SignalSpec> {
        None
    }

    fn decode<B: Bytestream>(&mut self, packet: &mut Packet<'_, B>, buf: &mut AudioBuffer<i32>) -> Result<()> {
        unsupported_error("Decoding is unsupported.")
    }
}


#[cfg(test)]
mod tests {
    use std::fs::File;
    use super::{Format, FormatReader, Wav, ProbeDepth};

    #[test]
    fn it_works() {
        let file = Box::new(File::open("samples/wav/metadata_pcm32le.wav").unwrap());
        let mut reader = Wav::open(file);
        let probe_info = reader.probe(ProbeDepth::Deep).unwrap();
    }
}
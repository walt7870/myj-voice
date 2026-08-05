use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AudioFormat {
    Mp3,
    Pcm16k,
    Pcm,
    Opus,
    Speex,
}

impl AudioFormat {
    pub fn parse(value: Option<&str>) -> Result<Self, AudioFormatError> {
        match value.unwrap_or("mp3") {
            "mp3" => Ok(Self::Mp3),
            "pcm16k" => Ok(Self::Pcm16k),
            value => Err(AudioFormatError::unsupported(value)),
        }
    }

    pub fn parse_profile(value: Option<&str>) -> Result<Self, AudioProfileError> {
        match value.unwrap_or("mp3") {
            "mp3" => Ok(Self::Mp3),
            "pcm" => Ok(Self::Pcm),
            "opus" => Ok(Self::Opus),
            "speex" => Ok(Self::Speex),
            value => Err(AudioProfileError::unsupported_format(value)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Pcm16k => "pcm16k",
            Self::Pcm => "pcm",
            Self::Opus => "opus",
            Self::Speex => "speex",
        }
    }

    /// Legacy XFYun mapping for values accepted by `AudioFormat::parse`.
    ///
    /// New `AudioProfile` values must be resolved through a provider adapter.
    pub fn xfyun_encoding(self) -> &'static str {
        match self {
            Self::Mp3 => "lame",
            Self::Pcm16k => "raw",
            Self::Pcm | Self::Opus | Self::Speex => "unsupported",
        }
    }

    /// Legacy XFYun IAT sample rate for values accepted by `AudioFormat::parse`.
    ///
    /// New `AudioProfile` values must be resolved through a provider adapter.
    pub fn iat_sample_rate(self) -> u32 {
        match self {
            Self::Mp3 | Self::Pcm16k => 16_000,
            Self::Pcm | Self::Opus | Self::Speex => 0,
        }
    }

    /// Legacy XFYun TTS sample rate for values accepted by `AudioFormat::parse`.
    ///
    /// New `AudioProfile` values must be resolved through a provider adapter.
    pub fn tts_sample_rate(self, provider: &str) -> u32 {
        match self {
            Self::Mp3 => match provider.trim() {
                "standard" | "online" => 16_000,
                _ => 24_000,
            },
            Self::Pcm16k => 16_000,
            Self::Pcm | Self::Opus | Self::Speex => 0,
        }
    }

    pub fn channels(self) -> u8 {
        1
    }

    pub fn bit_depth(self) -> Option<u8> {
        matches!(self, Self::Pcm16k | Self::Pcm).then_some(16)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AudioSampleRate {
    Hz8000,
    Hz16000,
    Hz24000,
}

impl AudioSampleRate {
    pub fn parse(value: Option<&str>) -> Result<Self, AudioProfileError> {
        match value.unwrap_or("16000") {
            "8000" => Ok(Self::Hz8000),
            "16000" => Ok(Self::Hz16000),
            "24000" => Ok(Self::Hz24000),
            value => Err(AudioProfileError::unsupported_rate(value)),
        }
    }

    pub const fn hz(self) -> u32 {
        match self {
            Self::Hz8000 => 8_000,
            Self::Hz16000 => 16_000,
            Self::Hz24000 => 24_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AudioProfile {
    pub format: AudioFormat,
    pub sample_rate: AudioSampleRate,
}

impl AudioProfile {
    pub const fn new(format: AudioFormat, sample_rate: AudioSampleRate) -> Self {
        Self {
            format,
            sample_rate,
        }
    }

    pub const fn pcm(sample_rate: AudioSampleRate) -> Self {
        Self::new(AudioFormat::Pcm, sample_rate)
    }

    pub const fn channels(self) -> u8 {
        1
    }

    pub const fn bit_depth(self) -> Option<u8> {
        match self.format {
            AudioFormat::Pcm16k | AudioFormat::Pcm => Some(16),
            AudioFormat::Mp3 | AudioFormat::Opus | AudioFormat::Speex => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceConnectionFormats {
    pub input: AudioFormat,
    pub output: AudioFormat,
}

impl VoiceConnectionFormats {
    pub fn from_query(input: Option<&str>, output: Option<&str>) -> Result<Self, AudioFormatError> {
        Ok(Self {
            input: AudioFormat::parse(input)?,
            output: AudioFormat::parse(output)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceConnectionAudio {
    pub input: AudioProfile,
    pub output: AudioProfile,
}

impl VoiceConnectionAudio {
    pub fn from_query(
        input_format: Option<&str>,
        input_rate: Option<&str>,
        output_format: Option<&str>,
        output_rate: Option<&str>,
    ) -> Result<Self, AudioProfileError> {
        let input = AudioProfile::new(
            AudioFormat::parse_profile(input_format)?,
            AudioSampleRate::parse(input_rate)?,
        );
        let output = AudioProfile::new(
            AudioFormat::parse_profile(output_format)?,
            AudioSampleRate::parse(output_rate)?,
        );

        Self::validate(input)?;
        Self::validate(output)?;

        Ok(Self { input, output })
    }

    fn validate(profile: AudioProfile) -> Result<(), AudioProfileError> {
        if profile.format == AudioFormat::Speex && profile.sample_rate == AudioSampleRate::Hz24000 {
            return Err(AudioProfileError::UnsupportedSpeexRate {
                sample_rate: profile.sample_rate,
            });
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFormatError {
    value: String,
}

impl AudioFormatError {
    fn unsupported(value: &str) -> Self {
        Self {
            value: value.to_string(),
        }
    }

    pub fn code(&self) -> &'static str {
        "unsupported_audio_format"
    }
}

impl fmt::Display for AudioFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported audio format '{}'; expected mp3 or pcm16k",
            self.value
        )
    }
}

impl std::error::Error for AudioFormatError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioProfileError {
    UnsupportedFormat { value: String },
    UnsupportedRate { value: String },
    UnsupportedSpeexRate { sample_rate: AudioSampleRate },
}

impl AudioProfileError {
    fn unsupported_format(value: &str) -> Self {
        Self::UnsupportedFormat {
            value: value.to_string(),
        }
    }

    fn unsupported_rate(value: &str) -> Self {
        Self::UnsupportedRate {
            value: value.to_string(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedFormat { .. } => "unsupported_audio_format",
            Self::UnsupportedRate { .. } | Self::UnsupportedSpeexRate { .. } => {
                "unsupported_audio_rate"
            }
        }
    }
}

impl fmt::Display for AudioProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat { value } => write!(
                formatter,
                "unsupported audio format '{}'; expected mp3, pcm, opus, or speex",
                value
            ),
            Self::UnsupportedRate { value } => write!(
                formatter,
                "unsupported audio rate '{}'; expected 8000, 16000, or 24000",
                value
            ),
            Self::UnsupportedSpeexRate { sample_rate } => write!(
                formatter,
                "unsupported audio rate '{}' for Speex; Speex only supports 8000 or 16000",
                sample_rate.hz()
            ),
        }
    }
}

impl std::error::Error for AudioProfileError {}

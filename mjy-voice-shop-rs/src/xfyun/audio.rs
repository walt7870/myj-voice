use std::fmt;

use crate::domain::audio::{AudioFormat, AudioProfile, AudioSampleRate};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IatProvider {
    SuperSmart,
    Standard,
}

impl IatProvider {
    pub fn parse(value: &str) -> Result<Self, AudioProviderError> {
        match value.trim() {
            "super_smart" => Ok(Self::SuperSmart),
            "standard" => Ok(Self::Standard),
            _ => Err(AudioProviderError::new("iat", value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TtsProvider {
    SuperSmart,
    Standard,
}

impl TtsProvider {
    pub fn parse(value: &str) -> Result<Self, AudioProviderError> {
        match value.trim() {
            "super_smart" => Ok(Self::SuperSmart),
            "standard" | "online" => Ok(Self::Standard),
            _ => Err(AudioProviderError::new("tts", value)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioProviderError {
    direction: &'static str,
    value: String,
}

impl AudioProviderError {
    fn new(direction: &'static str, value: &str) -> Self {
        Self {
            direction,
            value: value.to_string(),
        }
    }

    pub fn direction(&self) -> &'static str {
        self.direction
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for AudioProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let expected = match self.direction {
            "iat" => "super_smart or standard",
            "tts" => "super_smart, standard, or online",
            _ => "a configured provider",
        };

        write!(
            formatter,
            "unsupported {} provider '{}'; expected {}",
            self.direction, self.value, expected
        )
    }
}

impl std::error::Error for AudioProviderError {}

const IAT_SUPER_SMART_PROFILES: &[AudioProfile] = &[
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
];

const IAT_STANDARD_PROFILES: &[AudioProfile] = &[
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
];

const TTS_SUPER_SMART_PROFILES: &[AudioProfile] = &[
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000),
    AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
];

const TTS_STANDARD_PROFILES: &[AudioProfile] = &[
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
    AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
    AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
];

pub fn supported_iat_profiles(provider: IatProvider) -> &'static [AudioProfile] {
    match provider {
        IatProvider::SuperSmart => IAT_SUPER_SMART_PROFILES,
        IatProvider::Standard => IAT_STANDARD_PROFILES,
    }
}

pub fn supported_tts_profiles(provider: TtsProvider) -> &'static [AudioProfile] {
    match provider {
        TtsProvider::SuperSmart => TTS_SUPER_SMART_PROFILES,
        TtsProvider::Standard => TTS_STANDARD_PROFILES,
    }
}

pub fn iat_supports(provider: IatProvider, profile: AudioProfile) -> bool {
    supported_iat_profiles(provider).contains(&profile)
}

pub fn tts_supports(provider: TtsProvider, profile: AudioProfile) -> bool {
    supported_tts_profiles(provider).contains(&profile)
}

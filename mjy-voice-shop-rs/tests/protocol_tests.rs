use mjy_voice_shop_rs::config::{
    available_super_smart_voices, mock_providers_from_env_value, AppConfig,
};
use mjy_voice_shop_rs::domain::audio::{
    AudioFormat, AudioProfile, AudioSampleRate, VoiceConnectionAudio, VoiceConnectionFormats,
};
use mjy_voice_shop_rs::domain::device_auth::{issue_device_token, verify_device_token};
use mjy_voice_shop_rs::domain::matching::{match_products, Product};
use mjy_voice_shop_rs::domain::order::create_mock_order;
use mjy_voice_shop_rs::web::{
    classify_iat_error, classify_tts_error, decode_audio_packet, decode_live_audio_packet,
    friendly_error_message, is_interrupt_word_match, should_suppress_empty_asr,
    LIVE_IAT_SESSION_TIMEOUT, MAX_AUDIO_BASE64_BYTES, MAX_DECODED_AUDIO_BYTES,
};
use mjy_voice_shop_rs::xfyun::audio::{
    iat_supports, supported_iat_profiles, supported_tts_profiles, tts_supports, IatProvider,
    TtsProvider,
};
use mjy_voice_shop_rs::xfyun::auth::build_signed_ws_url;
use mjy_voice_shop_rs::xfyun::iat::{
    build_iat_frame, build_iat_frame_for_format, build_iat_frame_for_profile,
    build_iat_segment_frames, build_iat_segment_frames_for_profile, build_standard_iat_frame,
    merge_iat_text, parse_iat_text, parse_iat_text_for_provider, parse_standard_iat_text,
    validate_input_packet, IatFrameKind, IatUpstreamError, IatUpstreamErrorKind,
    MAX_IAT_PACKET_BYTES, STANDARD_IAT_MAX_RAW_FRAME_BYTES,
};
use mjy_voice_shop_rs::xfyun::llm::{
    build_chat_payload, parse_chat_chunk, split_complete_sentences, ChatMessage,
};
use mjy_voice_shop_rs::xfyun::tts::{
    build_standard_tts_payload, build_standard_tts_payload_for_format,
    build_standard_tts_payload_for_profile, build_tts_payload, build_tts_payload_for_format,
    build_tts_payload_for_profile, couple_tts_text_io, forward_standard_tts_audio_frame,
    forward_tts_audio_frame, parse_standard_tts_audio, parse_standard_tts_audio_frame,
    parse_tts_audio, parse_tts_audio_frame, precheck_standard_tts_provider_audio,
    run_tts_stream_session, stream_audio_profile_chunks,
    stream_super_smart_tts_text_frames_for_profile, tts_encoding, StandardTtsPacketizer,
    TtsAudioChunk, TtsPacketizationError, TtsPacketizationErrorKind, TtsStreamProgress,
    TtsTextFrame, TtsUpstreamError, TtsUpstreamErrorKind, MAX_STANDARD_OPUS_PACKET_BYTES,
    MAX_STANDARD_TTS_PROVIDER_BLOCK_BASE64_BYTES, MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES,
};

struct CancellationGuard(Arc<AtomicBool>);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn signs_xfyun_websocket_url_with_standard_query_shape() {
    let signed = build_signed_ws_url(
        "wss://maas-api.cn-huabei-1.xf-yun.com/v1.1/chat",
        "api-key",
        "api-secret",
        "Tue, 07 Jul 2026 03:00:00 GMT",
    )
    .unwrap();

    let url = url::Url::parse(&signed).unwrap();
    let auth = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        url.query_pairs()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1
            .as_bytes(),
    )
    .unwrap();
    let auth = String::from_utf8(auth).unwrap();

    assert_eq!(url.host_str().unwrap(), "maas-api.cn-huabei-1.xf-yun.com");
    assert_eq!(url.path(), "/v1.1/chat");
    assert_eq!(
        url.query_pairs().find(|(k, _)| k == "host").unwrap().1,
        "maas-api.cn-huabei-1.xf-yun.com"
    );
    assert!(auth.contains("api_key=\"api-key\""));
    assert!(auth.contains("headers=\"host date request-line\""));
    assert!(auth.contains("signature=\""));
}

#[test]
fn builds_iat_first_and_last_frames_for_pcm_audio() {
    let first = build_iat_frame("048c5dc4", IatFrameKind::First, &[1, 2]).unwrap();
    let last = build_iat_frame("048c5dc4", IatFrameKind::Last, &[]).unwrap();

    assert_eq!(first["header"]["status"], 0);
    assert_eq!(first["header"]["app_id"], "048c5dc4");
    assert_eq!(first["parameter"]["iat"]["domain"], "slm");
    assert_eq!(first["payload"]["audio"]["sample_rate"], 16000);
    assert_eq!(first["payload"]["audio"]["encoding"], "raw");
    assert_eq!(first["payload"]["audio"]["audio"], "AQI=");
    assert_eq!(last["header"]["status"], 2);
}

#[test]
fn builds_standard_iat_speex_frames_with_open_source_frame_sizes() {
    let nb = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000);
    let wb = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let nb_frame = build_standard_iat_frame("app", IatFrameKind::First, &[1; 38], nb).unwrap();
    let wb_frame = build_standard_iat_frame("app", IatFrameKind::First, &[1; 60], wb).unwrap();

    assert_eq!(nb_frame["data"]["encoding"], "speex");
    assert_eq!(nb_frame["business"]["speex_size"], 38);
    assert_eq!(wb_frame["data"]["encoding"], "speex-wb");
    assert_eq!(wb_frame["business"]["speex_size"], 60);
    assert_eq!(wb_frame["data"]["format"], "audio/L16;rate=16000");
}

#[test]
fn rejects_wrong_speex_packet_size_without_buffering() {
    let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let error = validate_input_packet(profile, &[0; 59]).unwrap_err();

    assert_eq!(error.code(), "invalid_audio_packet");
    assert!(build_standard_iat_frame("app", IatFrameKind::First, &[0; 60], profile).is_ok());
    assert!(build_standard_iat_frame("app", IatFrameKind::Continue, &[0; 60], profile).is_ok());
    assert_eq!(
        build_standard_iat_frame("app", IatFrameKind::Continue, &[0; 59], profile)
            .unwrap_err()
            .downcast_ref::<mjy_voice_shop_rs::xfyun::iat::AudioPacketError>()
            .unwrap()
            .code(),
        "invalid_audio_packet"
    );
}

#[test]
fn whole_speex_recognition_accepts_one_packet_and_rejects_multiple_packets() {
    let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);

    let frames =
        build_iat_segment_frames_for_profile("app", &[0; 60], 1280, profile, IatProvider::Standard)
            .unwrap();
    let error = build_iat_segment_frames_for_profile(
        "app",
        &[0; 120],
        1280,
        profile,
        IatProvider::Standard,
    )
    .unwrap_err();

    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["data"]["status"], 0);
    assert_eq!(frames[1]["data"]["status"], 2);
    assert_eq!(
        error
            .downcast_ref::<mjy_voice_shop_rs::xfyun::iat::AudioPacketError>()
            .unwrap()
            .code(),
        "invalid_audio_packet"
    );
}

#[test]
fn builds_standard_pcm_and_mp3_profiles_at_supported_rates() {
    for rate in [AudioSampleRate::Hz8000, AudioSampleRate::Hz16000] {
        let hz = rate.hz();
        let pcm = build_standard_iat_frame(
            "app",
            IatFrameKind::First,
            &[1, 2],
            AudioProfile::new(AudioFormat::Pcm, rate),
        )
        .unwrap();
        let mp3 = build_standard_iat_frame(
            "app",
            IatFrameKind::First,
            &[1],
            AudioProfile::new(AudioFormat::Mp3, rate),
        )
        .unwrap();

        assert_eq!(pcm["data"]["encoding"], "raw");
        assert_eq!(mp3["data"]["encoding"], "lame");
        assert_eq!(pcm["data"]["format"], format!("audio/L16;rate={hz}"));
        assert_eq!(mp3["data"]["format"], format!("audio/L16;rate={hz}"));
    }
}

#[test]
fn standard_iat_enforces_the_13000_byte_base64_frame_limit_before_upstream() {
    let profile = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let accepted = vec![0; STANDARD_IAT_MAX_RAW_FRAME_BYTES];
    let rejected = vec![0; STANDARD_IAT_MAX_RAW_FRAME_BYTES + 1];

    let frame = build_iat_frame_for_profile(
        "app",
        IatFrameKind::First,
        &accepted,
        profile,
        IatProvider::Standard,
    )
    .unwrap();
    let error = build_iat_frame_for_profile(
        "app",
        IatFrameKind::First,
        &rejected,
        profile,
        IatProvider::Standard,
    )
    .unwrap_err();

    assert!(frame["data"]["audio"].as_str().unwrap().len() <= 13_000);
    let error = error
        .downcast_ref::<mjy_voice_shop_rs::xfyun::iat::AudioPacketError>()
        .unwrap();
    assert_eq!(error.code(), "invalid_audio_packet");
    assert!(error.to_string().contains("standard"));
    assert!(error.to_string().contains("9750"));

    assert!(build_iat_frame_for_profile(
        "app",
        IatFrameKind::First,
        &rejected,
        profile,
        IatProvider::SuperSmart,
    )
    .is_ok());
}

#[test]
fn whole_audio_recognition_rejects_an_empty_segment_before_connecting() {
    let profile = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let error =
        build_iat_segment_frames_for_profile("app", &[], 1280, profile, IatProvider::Standard)
            .unwrap_err();

    assert_eq!(
        error
            .downcast_ref::<mjy_voice_shop_rs::xfyun::iat::AudioPacketError>()
            .unwrap()
            .code(),
        "invalid_audio_packet"
    );
}

#[test]
fn standard_iat_frames_follow_first_continue_last_schema() {
    let profile = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000);
    let first = build_standard_iat_frame("app", IatFrameKind::First, &[1, 2], profile).unwrap();
    let continued =
        build_standard_iat_frame("app", IatFrameKind::Continue, &[3, 4], profile).unwrap();
    let last = build_standard_iat_frame("app", IatFrameKind::Last, &[], profile).unwrap();

    assert_eq!(first["common"]["app_id"], "app");
    assert_eq!(first["business"]["language"], "zh_cn");
    assert_eq!(first["business"]["domain"], "iat");
    assert_eq!(first["business"]["accent"], "mandarin");
    assert_eq!(first["business"]["dwa"], "wpgs");
    assert_eq!(first["data"]["status"], 0);
    assert!(continued.get("common").is_none());
    assert!(continued.get("business").is_none());
    assert_eq!(continued["data"]["status"], 1);
    assert!(last.get("common").is_none());
    assert!(last.get("business").is_none());
    assert_eq!(last["data"]["status"], 2);
    assert_eq!(last["data"]["audio"], "");
}

#[test]
fn rejects_profiles_outside_the_selected_iat_provider_matrix() {
    for profile in [
        AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz24000),
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000),
    ] {
        let error = build_iat_frame_for_profile(
            "app",
            IatFrameKind::First,
            &[1, 2],
            profile,
            IatProvider::Standard,
        )
        .unwrap_err();
        let packet_error = error.downcast_ref::<mjy_voice_shop_rs::xfyun::iat::AudioPacketError>();
        assert_eq!(packet_error.unwrap().code(), "unsupported_audio_profile");
    }
}

#[test]
fn provider_aware_iat_builder_selects_private_and_standard_schemas() {
    let profile = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let private = build_iat_frame_for_profile(
        "app",
        IatFrameKind::First,
        &[1],
        profile,
        IatProvider::SuperSmart,
    )
    .unwrap();
    let standard = build_iat_frame_for_profile(
        "app",
        IatFrameKind::First,
        &[1],
        profile,
        IatProvider::Standard,
    )
    .unwrap();

    assert_eq!(private["payload"]["audio"]["sample_rate"], 16000);
    assert_eq!(private["payload"]["audio"]["encoding"], "lame");
    assert!(private.get("data").is_none());
    assert_eq!(standard["data"]["encoding"], "lame");
    assert!(standard.get("payload").is_none());
}

#[test]
fn validates_iat_packet_boundaries_without_accumulating_audio() {
    let pcm = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000);
    let mp3 = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let speex_nb = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000);

    assert_eq!(
        validate_input_packet(pcm, &[1]).unwrap_err().code(),
        "invalid_audio_packet"
    );
    assert_eq!(
        validate_input_packet(mp3, &[]).unwrap_err().code(),
        "invalid_audio_packet"
    );
    assert_eq!(
        validate_input_packet(speex_nb, &[0; 39])
            .unwrap_err()
            .code(),
        "invalid_audio_packet"
    );
    assert_eq!(
        validate_input_packet(mp3, &vec![0; MAX_IAT_PACKET_BYTES + 1])
            .unwrap_err()
            .code(),
        "invalid_audio_packet"
    );
}

#[test]
fn empty_standard_audio_is_only_valid_for_last_frame() {
    let profile = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000);

    assert!(build_standard_iat_frame("app", IatFrameKind::First, &[], profile).is_err());
    assert!(build_standard_iat_frame("app", IatFrameKind::Continue, &[], profile).is_err());
    assert!(build_standard_iat_frame("app", IatFrameKind::Last, &[], profile).is_ok());
}

#[test]
fn builds_iat_payload_for_mp3_and_pcm16k() {
    let mp3 =
        build_iat_frame_for_format("app", IatFrameKind::First, &[1], AudioFormat::Mp3).unwrap();
    let pcm = build_iat_frame_for_format("app", IatFrameKind::First, &[1, 2], AudioFormat::Pcm16k)
        .unwrap();

    assert_eq!(mp3["payload"]["audio"]["encoding"], "lame");
    assert_eq!(pcm["payload"]["audio"]["encoding"], "raw");
    assert_eq!(pcm["payload"]["audio"]["sample_rate"], 16000);
}

#[test]
fn builds_iat_segment_as_first_continue_and_empty_last_frames() {
    let frames = build_iat_segment_frames("048c5dc4", &[1, 2, 3, 4, 5, 6], 2).unwrap();

    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0]["header"]["status"], 0);
    assert_eq!(frames[0]["payload"]["audio"]["audio"], "AQI=");
    assert_eq!(frames[1]["header"]["status"], 1);
    assert_eq!(frames[1]["payload"]["audio"]["audio"], "AwQ=");
    assert_eq!(frames[2]["header"]["status"], 1);
    assert_eq!(frames[2]["payload"]["audio"]["audio"], "BQY=");
    assert_eq!(frames[3]["header"]["status"], 2);
    assert_eq!(frames[3]["payload"]["audio"]["audio"], "");
}

#[test]
fn mock_providers_default_to_real_provider_unless_explicitly_enabled() {
    assert!(!mock_providers_from_env_value(None));
    assert!(!mock_providers_from_env_value(Some("0")));
    assert!(!mock_providers_from_env_value(Some("false")));
    assert!(mock_providers_from_env_value(Some("1")));
    assert!(mock_providers_from_env_value(Some("true")));
}

#[test]
fn deserializes_saved_config_without_voice_display_name() {
    let raw = r#"{
        "app_id":"048c5dc4",
        "api_key":"key",
        "api_secret":"secret",
        "iat_endpoint":"ws://iat.xf-yun.com/v1",
        "tts_endpoint":"wss://cbm01.cn-huabei-1.xf-yun.com/v1/private/mcd9m97e6",
        "tts_voice":"x6_lingfeibo_pro",
        "llm_endpoint":"wss://maas-api.cn-huabei-1.xf-yun.com/v1.1/chat",
        "llm_model":"xopdeepseekv4flash",
        "temperature":0.4,
        "max_tokens":1024,
        "role_prompt":"role",
        "analysis_prompt":"analysis",
        "mock_providers":false
    }"#;

    let config: AppConfig = serde_json::from_str(raw).unwrap();

    assert_eq!(config.tts_voice_name, "聆小璇");
    assert_eq!(config.iat_provider, "super_smart");
    assert_eq!(config.to_public().iat_provider, "super_smart");
}

#[test]
fn standard_iat_supports_speex_but_not_opus() {
    assert!(iat_supports(
        IatProvider::Standard,
        AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000)
    ));
    assert!(iat_supports(
        IatProvider::Standard,
        AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000)
    ));
    assert!(!iat_supports(
        IatProvider::Standard,
        AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000)
    ));
    assert!(!iat_supports(
        IatProvider::Standard,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000)
    ));
}

#[test]
fn standard_tts_supports_open_codecs_at_8k_and_16k() {
    for format in [
        AudioFormat::Pcm,
        AudioFormat::Mp3,
        AudioFormat::Opus,
        AudioFormat::Speex,
    ] {
        assert!(tts_supports(
            TtsProvider::Standard,
            AudioProfile::new(format, AudioSampleRate::Hz8000)
        ));
        assert!(tts_supports(
            TtsProvider::Standard,
            AudioProfile::new(format, AudioSampleRate::Hz16000)
        ));
        assert!(!tts_supports(
            TtsProvider::Standard,
            AudioProfile::new(format, AudioSampleRate::Hz24000)
        ));
    }
}

#[test]
fn private_providers_expose_only_verified_profiles() {
    assert!(iat_supports(
        IatProvider::SuperSmart,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000)
    ));
    assert!(iat_supports(
        IatProvider::SuperSmart,
        AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000)
    ));
    assert!(!iat_supports(
        IatProvider::SuperSmart,
        AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000)
    ));
    assert!(tts_supports(
        TtsProvider::SuperSmart,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000)
    ));
    assert!(!tts_supports(
        TtsProvider::SuperSmart,
        AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000)
    ));
}

#[test]
fn provider_capability_lists_exactly_match_the_verified_matrix() {
    assert_eq!(
        supported_iat_profiles(IatProvider::SuperSmart),
        &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
        ]
    );
    assert_eq!(
        supported_iat_profiles(IatProvider::Standard),
        &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ]
    );
    assert_eq!(
        supported_tts_profiles(TtsProvider::SuperSmart),
        &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
        ]
    );
    assert_eq!(
        supported_tts_profiles(TtsProvider::Standard),
        &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ]
    );
}

#[test]
fn provider_parsing_is_exact_with_tts_online_as_standard_alias() {
    assert_eq!(
        IatProvider::parse("super_smart").unwrap(),
        IatProvider::SuperSmart
    );
    assert_eq!(
        IatProvider::parse(" standard ").unwrap(),
        IatProvider::Standard
    );
    assert_eq!(
        TtsProvider::parse("super_smart").unwrap(),
        TtsProvider::SuperSmart
    );
    assert_eq!(
        TtsProvider::parse("standard").unwrap(),
        TtsProvider::Standard
    );
    assert_eq!(TtsProvider::parse("online").unwrap(), TtsProvider::Standard);

    for value in ["SUPER_SMART", "online"] {
        assert!(IatProvider::parse(value).is_err());
    }
    for value in ["STANDARD", "other"] {
        assert!(TtsProvider::parse(value).is_err());
    }
}

#[test]
fn unknown_provider_parse_errors_identify_direction_and_value() {
    let iat_error = IatProvider::parse("unsupported").unwrap_err();
    assert_eq!(iat_error.direction(), "iat");
    assert_eq!(iat_error.value(), "unsupported");
    assert_eq!(
        iat_error.to_string(),
        "unsupported iat provider 'unsupported'; expected super_smart or standard"
    );

    let tts_error = TtsProvider::parse("unsupported").unwrap_err();
    assert_eq!(tts_error.direction(), "tts");
    assert_eq!(tts_error.value(), "unsupported");
    assert_eq!(
        tts_error.to_string(),
        "unsupported tts provider 'unsupported'; expected super_smart, standard, or online"
    );
}

#[test]
fn config_preserves_unknown_iat_provider_for_explicit_validation() {
    let mut serialized = serde_json::to_value(AppConfig::default_from_env()).unwrap();
    serialized["iat_provider"] = serde_json::json!("unsupported");

    let config: AppConfig = serde_json::from_value(serialized).unwrap();

    assert_eq!(config.iat_provider, "unsupported");
    assert_eq!(
        IatProvider::parse(&config.iat_provider)
            .unwrap_err()
            .direction(),
        "iat"
    );
}

#[test]
fn tts_no_interrupt_defaults_on_and_is_exposed_publicly() {
    let raw = r#"{
        "app_id":"048c5dc4",
        "api_key":"key",
        "api_secret":"secret",
        "iat_endpoint":"ws://iat.xf-yun.com/v1",
        "tts_endpoint":"wss://cbm01.cn-huabei-1.xf-yun.com/v1/private/mcd9m97e6",
        "tts_voice":"x6_lingfeibo_pro",
        "llm_endpoint":"wss://maas-api.cn-huabei-1.xf-yun.com/v1.1/chat",
        "llm_model":"xopdeepseekv4flash",
        "temperature":0.4,
        "max_tokens":1024,
        "role_prompt":"role",
        "analysis_prompt":"analysis",
        "mock_providers":false
    }"#;

    let config: AppConfig = serde_json::from_str(raw).unwrap();
    let public = config.to_public();

    assert!(config.tts_no_interrupt);
    assert!(public.tts_no_interrupt);
    assert_eq!(config.tts_interrupt_word, "停一下");
    assert_eq!(public.tts_interrupt_word, "停一下");
}

#[test]
fn voice_connection_formats_default_to_mp3() {
    let formats = VoiceConnectionFormats::from_query(None, None).unwrap();

    assert_eq!(formats.input, AudioFormat::Mp3);
    assert_eq!(formats.output, AudioFormat::Mp3);
}

#[test]
fn voice_connection_formats_accept_exact_supported_values() {
    let formats = VoiceConnectionFormats::from_query(Some("pcm16k"), Some("mp3")).unwrap();

    assert_eq!(formats.input, AudioFormat::Pcm16k);
    assert_eq!(formats.output, AudioFormat::Mp3);
    assert_eq!(AudioFormat::Pcm16k.iat_sample_rate(), 16_000);
    assert_eq!(AudioFormat::Pcm16k.tts_sample_rate("super_smart"), 16_000);
    assert_eq!(AudioFormat::Mp3.tts_sample_rate("super_smart"), 24_000);
    assert_eq!(AudioFormat::Mp3.tts_sample_rate("standard"), 16_000);
    assert_eq!(AudioFormat::Pcm16k.channels(), 1);
    assert_eq!(AudioFormat::Pcm16k.bit_depth(), Some(16));
}

#[test]
fn voice_connection_formats_reject_aliases_and_case_variants() {
    for value in ["PCM16K", "pcm", "wav", "audio/mpeg"] {
        assert_eq!(
            VoiceConnectionFormats::from_query(Some(value), None)
                .unwrap_err()
                .code(),
            "unsupported_audio_format"
        );
    }
}

#[test]
fn voice_audio_defaults_to_mp3_16k_in_both_directions() {
    let audio = VoiceConnectionAudio::from_query(None, None, None, None).unwrap();

    assert_eq!(
        audio.input,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000)
    );
    assert_eq!(
        audio.output,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000)
    );
}

#[test]
fn voice_audio_accepts_independent_codec_and_rate_profiles() {
    let audio =
        VoiceConnectionAudio::from_query(Some("speex"), Some("8000"), Some("opus"), Some("16000"))
            .unwrap();

    assert_eq!(audio.input.format, AudioFormat::Speex);
    assert_eq!(audio.input.sample_rate.hz(), 8_000);
    assert_eq!(audio.output.format, AudioFormat::Opus);
    assert_eq!(audio.output.sample_rate.hz(), 16_000);
    assert_eq!(
        AudioProfile::pcm(AudioSampleRate::Hz8000).bit_depth(),
        Some(16)
    );
}

#[test]
fn voice_audio_rejects_unknown_format_rate_and_speex_24k() {
    assert_eq!(
        VoiceConnectionAudio::from_query(Some("wav"), None, None, None)
            .unwrap_err()
            .code(),
        "unsupported_audio_format"
    );
    assert_eq!(
        VoiceConnectionAudio::from_query(None, Some("44100"), None, None)
            .unwrap_err()
            .code(),
        "unsupported_audio_rate"
    );
    for error in [
        VoiceConnectionAudio::from_query(Some("speex"), Some("24000"), None, None).unwrap_err(),
        VoiceConnectionAudio::from_query(None, None, Some("speex"), Some("24000")).unwrap_err(),
    ] {
        assert_eq!(error.code(), "unsupported_audio_rate");
        let message = error.to_string();
        assert!(message.contains("Speex"));
        assert!(message.contains("8000"));
        assert!(message.contains("16000"));
    }
}

#[test]
fn voice_audio_new_profiles_are_rejected_by_legacy_xfyun_helpers() {
    for format in [AudioFormat::Pcm, AudioFormat::Opus, AudioFormat::Speex] {
        assert_eq!(format.xfyun_encoding(), "unsupported");
        assert_eq!(format.iat_sample_rate(), 0);
        assert_eq!(format.tts_sample_rate("super_smart"), 0);
    }
}

#[test]
fn audio_profile_format_parsing_is_exact_and_excludes_legacy_pcm16k() {
    assert_eq!(AudioFormat::parse_profile(None).unwrap(), AudioFormat::Mp3);
    assert_eq!(
        AudioFormat::parse_profile(Some("pcm")).unwrap(),
        AudioFormat::Pcm
    );

    for value in ["PCM", "pcm16k"] {
        assert_eq!(
            AudioFormat::parse_profile(Some(value)).unwrap_err().code(),
            "unsupported_audio_format"
        );
    }

    assert_eq!(
        VoiceConnectionFormats::from_query(Some("pcm16k"), None)
            .unwrap()
            .input,
        AudioFormat::Pcm16k
    );
}

#[test]
fn interrupt_word_match_ignores_spacing_and_punctuation() {
    assert!(is_interrupt_word_match("停 一下。", "停一下"));
    assert!(!is_interrupt_word_match("先别停一下", "停一下"));
    assert!(!is_interrupt_word_match("停一下", ""));
}

#[test]
fn exposes_only_supported_super_smart_voices() {
    let voices = available_super_smart_voices();

    assert_eq!(
        voices,
        vec![
            ("聆小璇".to_string(), "x6_lingxiaoxuan_pro".to_string()),
            ("聆飞瀚".to_string(), "x6_lingfeihan_pro".to_string()),
        ]
    );

    let config = AppConfig::default_from_env();
    assert_eq!(config.tts_voice_name, "聆小璇");
    assert_eq!(config.tts_voice, "x6_lingxiaoxuan_pro");
}

#[test]
fn parses_iat_base64_result_text() {
    let result = serde_json::json!({"ws":[{"cw":[{"w":"我要"}]},{"cw":[{"w":"可乐"}]}]});
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        serde_json::to_string(&result).unwrap(),
    );
    let message = serde_json::json!({
        "header": {"code": 0, "status": 2},
        "payload": {"result": {"text": encoded}}
    });

    assert_eq!(parse_iat_text(&message).unwrap().text, "我要可乐");
    assert!(parse_iat_text(&message).unwrap().is_final);
}

#[test]
fn parses_iat_status_frame_without_result_as_empty_text() {
    let message = serde_json::json!({
        "header": {"code": 0, "status": 1}
    });

    let parsed = parse_iat_text(&message).unwrap();

    assert_eq!(parsed.text, "");
    assert!(!parsed.is_final);
}

#[test]
fn parses_standard_iat_result_text_and_data_status() {
    let message = serde_json::json!({
        "code": 0,
        "data": {
            "status": 2,
            "result": {
                "ws": [
                    {"cw": [{"w": "我要"}]},
                    {"cw": [{"w": "可乐"}]}
                ]
            }
        }
    });

    let parsed = parse_standard_iat_text(&message).unwrap();

    assert_eq!(parsed.text, "我要可乐");
    assert!(parsed.is_final);
}

#[test]
fn standard_iat_parser_rejects_nonzero_upstream_code() {
    let message = serde_json::json!({
        "code": 10105,
        "message": "illegal access",
        "data": {"status": 2}
    });

    let error = parse_standard_iat_text(&message).unwrap_err();

    assert!(error.to_string().contains("10105"));
    assert!(error.to_string().contains("illegal access"));
}

#[test]
fn classifies_iat_failures_without_string_matching() {
    let profile = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000);
    let invalid_packet =
        build_standard_iat_frame("app", IatFrameKind::First, &[1], profile).unwrap_err();
    let upstream_rejection = parse_standard_iat_text(&serde_json::json!({
        "code": 10163,
        "message": "unsupported sample_rate for speex encoding"
    }))
    .unwrap_err();
    let network = anyhow::anyhow!("connection reset");

    assert_eq!(classify_iat_error(&invalid_packet), "invalid_audio_packet");
    assert_eq!(
        classify_iat_error(&upstream_rejection),
        "upstream_audio_profile_rejected"
    );
    assert_eq!(classify_iat_error(&network), "asr_failed");
}

#[test]
fn only_explicit_audio_profile_rejections_get_the_upstream_profile_code() {
    let decode_rejection = parse_standard_iat_text(&serde_json::json!({
        "code": 10043,
        "message": "audio decoding failed"
    }))
    .unwrap_err();
    assert_eq!(
        classify_iat_error(&decode_rejection),
        "upstream_audio_profile_rejected"
    );

    let bare_rate_error = parse_standard_iat_text(&serde_json::json!({
        "code": 10163,
        "message": "unsupported rate 24000"
    }))
    .unwrap_err();
    assert_eq!(classify_iat_error(&bare_rate_error), "asr_failed");

    for (code, message, expected) in [
        (10006, "invalid vcn", "asr_failed"),
        (10006, "invalid speex", "asr_failed"),
        (10006, "invalid sample rate", "asr_failed"),
        (
            10006,
            "invalid audio rate",
            "upstream_audio_profile_rejected",
        ),
        (10007, "invalid aue", "upstream_audio_profile_rejected"),
        (
            10163,
            "unsupported encoding",
            "upstream_audio_profile_rejected",
        ),
        (10163, "invalid text parameter", "asr_failed"),
        (11200, "audio codec license denied", "asr_failed"),
    ] {
        let error = parse_standard_iat_text(&serde_json::json!({
            "code": code,
            "message": message
        }))
        .unwrap_err();
        assert_eq!(classify_iat_error(&error), expected, "{code}: {message}");
        let expected_kind = if expected == "upstream_audio_profile_rejected" {
            IatUpstreamErrorKind::AudioProfileRejected
        } else {
            IatUpstreamErrorKind::Other
        };
        assert_eq!(
            error.downcast_ref::<IatUpstreamError>().unwrap().kind,
            expected_kind,
            "{code}: {message}"
        );
    }

    for (code, message) in [
        (10105, "audio codec auth denied"),
        (11200, "audio codec license expired"),
        (10110, "audio codec quota exhausted"),
        (10200, "audio format server error"),
        (10106, "invalid parameter"),
        (10106, "audio codec QPS rate limit exceeded"),
        (10163, "request rate limit exceeded"),
        (10163, "request rate=100 exceeded"),
    ] {
        let error = parse_standard_iat_text(&serde_json::json!({
            "code": code,
            "message": message
        }))
        .unwrap_err();
        assert_eq!(
            classify_iat_error(&error),
            "asr_failed",
            "{code}: {message}"
        );
    }
}

#[test]
fn super_smart_iat_parser_preserves_typed_upstream_errors() {
    let profile = parse_iat_text(&serde_json::json!({
        "header": {"code": 10043, "message": "unsupported audio format", "status": 2}
    }))
    .unwrap_err();
    let auth = parse_iat_text(&serde_json::json!({
        "header": {"code": 10105, "message": "illegal access", "status": 2}
    }))
    .unwrap_err();

    let typed = profile.downcast_ref::<IatUpstreamError>().unwrap();
    assert_eq!(typed.provider, IatProvider::SuperSmart);
    assert_eq!(typed.code, 10043);
    assert_eq!(typed.kind, IatUpstreamErrorKind::AudioProfileRejected);
    assert_eq!(
        classify_iat_error(&profile),
        "upstream_audio_profile_rejected"
    );
    assert_eq!(classify_iat_error(&auth), "asr_failed");

    for (code, message, expected) in [
        (10006, "invalid vcn", "asr_failed"),
        (10006, "invalid speex", "asr_failed"),
        (10006, "invalid sample rate", "asr_failed"),
        (
            10006,
            "invalid audio rate",
            "upstream_audio_profile_rejected",
        ),
        (
            10163,
            "unsupported encoding",
            "upstream_audio_profile_rejected",
        ),
        (10163, "invalid text parameter", "asr_failed"),
        (11200, "audio codec license denied", "asr_failed"),
        (10105, "audio codec auth denied", "asr_failed"),
        (10110, "audio codec quota exhausted", "asr_failed"),
        (10200, "audio format server error", "asr_failed"),
        (10106, "audio codec QPS exceeded", "asr_failed"),
    ] {
        let error = parse_iat_text(&serde_json::json!({
            "header": {"code": code, "message": message, "status": 2}
        }))
        .unwrap_err();
        assert_eq!(classify_iat_error(&error), expected, "{code}: {message}");
        let expected_kind = if expected == "upstream_audio_profile_rejected" {
            IatUpstreamErrorKind::AudioProfileRejected
        } else {
            IatUpstreamErrorKind::Other
        };
        assert_eq!(
            error.downcast_ref::<IatUpstreamError>().unwrap().kind,
            expected_kind,
            "{code}: {message}"
        );
    }
}

#[test]
fn decodes_and_validates_audio_packets_for_all_ws_audio_paths() {
    let pcm = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000);
    let mp3 = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let speex = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let oversized = vec![0; MAX_IAT_PACKET_BYTES + 1];
    let oversized = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, oversized);
    let standard_oversized_frame = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![0; STANDARD_IAT_MAX_RAW_FRAME_BYTES + 1],
    );

    for error in [
        decode_audio_packet(None, pcm).unwrap_err(),
        decode_audio_packet(Some("not-base64***"), pcm).unwrap_err(),
        decode_audio_packet(Some(""), mp3).unwrap_err(),
        decode_audio_packet(Some("AQ=="), pcm).unwrap_err(),
        decode_audio_packet(Some(&oversized), mp3).unwrap_err(),
        decode_audio_packet(Some("AA=="), speex).unwrap_err(),
        decode_live_audio_packet(None, Some("AQI=")).unwrap_err(),
        decode_live_audio_packet(
            Some((mp3, IatProvider::Standard)),
            Some(&standard_oversized_frame),
        )
        .unwrap_err(),
    ] {
        assert_eq!(error.code(), "invalid_audio_packet");
    }

    assert_eq!(decode_audio_packet(Some("AQI="), pcm).unwrap(), vec![1, 2]);
    assert_eq!(
        decode_live_audio_packet(Some((pcm, IatProvider::Standard)), Some("AQI=")).unwrap(),
        vec![1, 2]
    );
    assert!(decode_live_audio_packet(
        Some((mp3, IatProvider::SuperSmart)),
        Some(&standard_oversized_frame),
    )
    .is_ok());
}

#[test]
fn encoded_audio_limit_is_checked_before_base64_decode() {
    assert_eq!(MAX_DECODED_AUDIO_BYTES, 64 * 1024);
    assert_eq!(MAX_AUDIO_BASE64_BYTES, 87_384);
    let boundary = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![0; MAX_DECODED_AUDIO_BYTES],
    );
    assert_eq!(boundary.len(), MAX_AUDIO_BASE64_BYTES);
    assert_eq!(
        decode_audio_packet(
            Some(&boundary),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000)
        )
        .unwrap()
        .len(),
        MAX_DECODED_AUDIO_BYTES
    );

    let oversized = format!("{boundary}A");
    let error = decode_audio_packet(
        Some(&oversized),
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    )
    .unwrap_err();
    assert_eq!(error.code(), "invalid_audio_packet");
    assert!(error.to_string().contains("encoded audio packet"));
}

#[test]
fn asr_logging_never_writes_recognized_text_or_raw_asr_log_files() {
    for (name, source) in [
        ("web", include_str!("../src/web/mod.rs")),
        ("iat", include_str!("../src/xfyun/iat.rs")),
    ] {
        assert!(!source.contains("text = %text"), "{name}");
        assert!(!source.contains("text={text}"), "{name}");
        assert!(!source.contains("text = %parsed.text"), "{name}");
        assert!(!source.contains("parsed.is_final, parsed.text"), "{name}");
        assert!(!source.contains("append_asr_log"), "{name}");
        assert!(!source.contains("append_iat_log"), "{name}");
        assert!(!source.contains("logs/asr.log"), "{name}");
    }
}

#[test]
fn provider_aware_iat_parser_keeps_aiges_and_standard_shapes() {
    let private_result = serde_json::json!({"ws":[{"cw":[{"w":"私有"}]}]});
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        serde_json::to_string(&private_result).unwrap(),
    );
    let private = serde_json::json!({
        "header": {"code": 0, "status": 2},
        "payload": {"result": {"text": encoded}}
    });
    let standard = serde_json::json!({
        "code": 0,
        "data": {
            "status": 2,
            "result": {"ws": [{"cw": [{"w": "标准"}]}]}
        }
    });

    assert_eq!(
        parse_iat_text_for_provider(&private, IatProvider::SuperSmart)
            .unwrap()
            .text,
        "私有"
    );
    assert_eq!(
        parse_iat_text_for_provider(&standard, IatProvider::Standard)
            .unwrap()
            .text,
        "标准"
    );
}

#[test]
fn merges_iat_wpgs_chunks_instead_of_overwriting_with_final_punctuation() {
    let text = merge_iat_text("", "买两瓶可乐和一瓶水");
    let text = merge_iat_text(&text, "。");

    assert_eq!(text, "买两瓶可乐和一瓶水。");
}

#[test]
fn merges_iat_full_replacement_without_duplicate_text() {
    let text = merge_iat_text("", "买两瓶可乐");
    let text = merge_iat_text(&text, "买两瓶可乐和一瓶水");

    assert_eq!(text, "买两瓶可乐和一瓶水");
}

#[test]
fn parses_only_the_top_iat_candidate_instead_of_repeating_alternatives() {
    let message = serde_json::json!({
        "code": 0,
        "data": {
            "status": 2,
            "result": {
                "ws": [{
                    "cw": [
                        {"w": "退一下吧。"},
                        {"w": "退一下吧"}
                    ]
                }]
            }
        }
    });

    assert_eq!(
        parse_standard_iat_text(&message).unwrap().text,
        "退一下吧。"
    );
}

#[test]
fn builds_llm_payload_with_selected_domain_and_messages() {
    let payload = build_chat_payload(
        "048c5dc4",
        "xopdeepseekv4flash",
        0.4,
        1024,
        vec![
            ChatMessage::system("你是玩偶助手"),
            ChatMessage::user("买两瓶可乐"),
        ],
    );

    assert_eq!(payload["parameter"]["chat"]["domain"], "xopdeepseekv4flash");
    assert_eq!(payload["payload"]["message"]["text"][0]["role"], "system");
    assert_eq!(
        payload["payload"]["message"]["text"][1]["content"],
        "买两瓶可乐"
    );
}

#[test]
fn parses_llm_content_and_keeps_reasoning_out_of_reply() {
    let chunk = serde_json::json!({
        "header": {"code": 0},
        "payload": {"choices": {"status": 2, "text": [{
            "content": "好的。",
            "reasoning_content": "内部推理"
        }]}}
    });
    let parsed = parse_chat_chunk(&chunk).unwrap();

    assert_eq!(parsed.content, "好的。");
    assert_eq!(parsed.reasoning_content.as_deref(), Some("内部推理"));
    assert!(parsed.is_final);
}

#[test]
fn splits_llm_delta_into_complete_short_sentences() {
    let mut buffer = String::new();
    let ready = split_complete_sentences(&mut buffer, "好的，我先帮你确认。还需要一点信息");

    assert_eq!(ready, vec!["好的，我先帮你确认。"]);
    assert_eq!(buffer, "还需要一点信息");
}

#[test]
fn builds_tts_payload_and_parses_audio_chunk() {
    let payload = build_tts_payload("048c5dc4", "x6_lingfeibo_pro", "好的，已为你确认。");
    let text = payload["payload"]["text"]["text"].as_str().unwrap();
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text).unwrap();

    assert_eq!(String::from_utf8(decoded).unwrap(), "好的，已为你确认。");
    assert_eq!(payload["parameter"]["tts"]["audio"]["encoding"], "lame");
    assert_eq!(payload["parameter"]["tts"]["audio"]["sample_rate"], 16000);

    let message = serde_json::json!({
        "header": {"code": 0},
        "payload": {"audio": {"audio": "AQID", "status": 2}}
    });
    let chunk = parse_tts_audio(&message).unwrap();
    assert_eq!(chunk.audio, vec![1, 2, 3]);
    assert!(chunk.is_last);
}

#[test]
fn builds_super_smart_tts_payload_for_mp3_and_pcm16k() {
    let mp3 = build_tts_payload_for_format("app", "voice", "你好", AudioFormat::Mp3);
    let pcm = build_tts_payload_for_format("app", "voice", "你好", AudioFormat::Pcm16k);

    assert_eq!(mp3["parameter"]["tts"]["audio"]["encoding"], "lame");
    assert_eq!(mp3["parameter"]["tts"]["audio"]["sample_rate"], 16000);
    assert_eq!(pcm["parameter"]["tts"]["audio"]["encoding"], "raw");
    assert_eq!(pcm["parameter"]["tts"]["audio"]["sample_rate"], 16000);
}

#[test]
fn super_smart_tts_uses_the_requested_native_profile() {
    let mp3_24 = build_tts_payload_for_profile(
        "app",
        "voice",
        "你好",
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000),
    )
    .unwrap();
    let pcm_16 = build_tts_payload_for_profile(
        "app",
        "voice",
        "你好",
        AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
    )
    .unwrap();

    assert_eq!(mp3_24["parameter"]["tts"]["audio"]["encoding"], "lame");
    assert_eq!(mp3_24["parameter"]["tts"]["audio"]["sample_rate"], 24000);
    assert_eq!(mp3_24["parameter"]["tts"]["audio"]["channels"], 1);
    assert_eq!(pcm_16["parameter"]["tts"]["audio"]["encoding"], "raw");
    assert_eq!(pcm_16["parameter"]["tts"]["audio"]["sample_rate"], 16000);
    assert_eq!(pcm_16["parameter"]["tts"]["audio"]["channels"], 1);
    assert_eq!(pcm_16["parameter"]["tts"]["audio"]["bit_depth"], 16);
}

#[test]
fn super_smart_tts_maps_mp3_at_8k() {
    let payload = build_tts_payload_for_profile(
        "app",
        "voice",
        "你好",
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
    )
    .unwrap();
    let audio = &payload["parameter"]["tts"]["audio"];

    assert_eq!(audio["encoding"], "lame");
    assert_eq!(audio["sample_rate"], 8000);
    assert_eq!(audio["channels"], 1);
    assert_eq!(audio["bit_depth"], 16);
}

#[test]
fn tts_rejects_profiles_outside_the_provider_matrix() {
    for (provider, profile) in [
        (
            TtsProvider::SuperSmart,
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        ),
        (
            TtsProvider::SuperSmart,
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
        ),
        (
            TtsProvider::Standard,
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000),
        ),
    ] {
        let error = tts_encoding(profile, provider).unwrap_err();
        assert_eq!(error.code(), "unsupported_audio_profile");
        assert!(error.to_string().contains(profile.format.as_str()));
        assert!(error
            .to_string()
            .contains(&profile.sample_rate.hz().to_string()));
    }
}

#[test]
fn skips_super_smart_tts_non_audio_frames() {
    let message = serde_json::json!({
        "header": {"code": 0, "status": 1},
        "payload": {"result_semantic": {"text": "metadata"}}
    });

    assert_eq!(parse_tts_audio_frame(&message).unwrap(), None);
}

#[test]
fn builds_standard_tts_payload_and_parses_data_audio_chunk() {
    let payload =
        build_standard_tts_payload("048c5dc4", "x4_lingxiaolu_em_v2", "好的，已为你确认。");
    let text = payload["data"]["text"].as_str().unwrap();
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text).unwrap();

    assert_eq!(payload["common"]["app_id"], "048c5dc4");
    assert_eq!(payload["business"]["vcn"], "x4_lingxiaolu_em_v2");
    assert_eq!(payload["business"]["aue"], "lame");
    assert_eq!(String::from_utf8(decoded).unwrap(), "好的，已为你确认。");

    let message = serde_json::json!({
        "code": 0,
        "data": {"audio": "AQID", "status": 2}
    });
    let chunk = parse_standard_tts_audio(&message).unwrap();
    assert_eq!(chunk.audio, vec![1, 2, 3]);
    assert!(chunk.is_last);
}

#[test]
fn builds_standard_tts_payload_for_mp3_and_pcm16k() {
    let mp3 = build_standard_tts_payload_for_format("app", "voice", "你好", AudioFormat::Mp3);
    let pcm = build_standard_tts_payload_for_format("app", "voice", "你好", AudioFormat::Pcm16k);

    assert_eq!(mp3["business"]["aue"], "lame");
    assert_eq!(pcm["business"]["aue"], "raw");
    assert_eq!(pcm["business"]["auf"], "audio/L16;rate=16000");
}

#[test]
fn standard_tts_maps_native_codecs_and_rates() {
    let cases = [
        (AudioFormat::Mp3, AudioSampleRate::Hz16000, "lame"),
        (AudioFormat::Pcm, AudioSampleRate::Hz8000, "raw"),
        (AudioFormat::Opus, AudioSampleRate::Hz8000, "opus"),
        (AudioFormat::Opus, AudioSampleRate::Hz16000, "opus-wb"),
        (
            AudioFormat::Speex,
            AudioSampleRate::Hz8000,
            "speex-org-nb;7",
        ),
        (
            AudioFormat::Speex,
            AudioSampleRate::Hz16000,
            "speex-org-wb;7",
        ),
    ];

    for (format, rate, aue) in cases {
        let profile = AudioProfile::new(format, rate);
        let payload =
            build_standard_tts_payload_for_profile("app", "voice", "你好", profile).unwrap();
        assert_eq!(payload["business"]["aue"], aue);
        assert_eq!(
            payload["business"]["auf"],
            format!("audio/L16;rate={}", rate.hz())
        );
    }
}

#[test]
fn standard_tts_maps_mp3_at_8k_with_sfl() {
    let payload = build_standard_tts_payload_for_profile(
        "app",
        "voice",
        "你好",
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
    )
    .unwrap();

    assert_eq!(payload["business"]["aue"], "lame");
    assert_eq!(payload["business"]["auf"], "audio/L16;rate=8000");
    assert_eq!(payload["business"]["sfl"], 1);
}

#[test]
fn standard_tts_only_sets_sfl_for_mp3() {
    let mp3 = build_standard_tts_payload_for_profile(
        "app",
        "voice",
        "你好",
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    )
    .unwrap();
    let opus = build_standard_tts_payload_for_profile(
        "app",
        "voice",
        "你好",
        AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
    )
    .unwrap();

    assert_eq!(mp3["business"]["sfl"], 1);
    assert!(opus["business"].get("sfl").is_none());
}

#[test]
fn standard_tts_response_preserves_native_packet_bytes() {
    let native_packet = vec![0, 255, 1, 128, 79, 103, 103, 83];
    let message = serde_json::json!({
        "code": 0,
        "data": {
            "audio": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &native_packet
            ),
            "status": 1
        }
    });

    let chunk = parse_standard_tts_audio(&message).unwrap();
    assert_eq!(chunk.audio, native_packet);
    assert!(!chunk.is_last);
}

#[test]
fn standard_tts_opus_splits_every_length_prefixed_packet_and_marks_only_the_last() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let provider_block = vec![0, 3, 1, 2, 3, 0, 2, 4, 5];

    let chunks = packetizer
        .push(TtsAudioChunk {
            audio: provider_block,
            is_last: true,
        })
        .unwrap();

    assert_eq!(
        chunks,
        vec![
            TtsAudioChunk {
                audio: vec![1, 2, 3],
                is_last: false,
            },
            TtsAudioChunk {
                audio: vec![4, 5],
                is_last: true,
            },
        ]
    );
}

#[test]
fn standard_tts_opus_reassembles_split_prefix_and_packet_without_buffering_the_stream() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();

    assert!(packetizer
        .push(TtsAudioChunk {
            audio: vec![0],
            is_last: false,
        })
        .unwrap()
        .is_empty());
    assert!(packetizer
        .push(TtsAudioChunk {
            audio: vec![3, 7],
            is_last: false,
        })
        .unwrap()
        .is_empty());
    let chunks = packetizer
        .push(TtsAudioChunk {
            audio: vec![8, 9, 0, 2, 10, 11],
            is_last: true,
        })
        .unwrap();

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].audio, vec![7, 8, 9]);
    assert!(!chunks[0].is_last);
    assert_eq!(chunks[1].audio, vec![10, 11]);
    assert!(chunks[1].is_last);
    assert_eq!(packetizer.buffered_bytes(), 0);
}

#[test]
fn standard_tts_opus_final_rejects_a_partial_prefix_or_packet_with_typed_errors() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    for (audio, expected_kind) in [
        (vec![0], TtsPacketizationErrorKind::TruncatedLengthPrefix),
        (vec![0, 3, 1, 2], TtsPacketizationErrorKind::TruncatedPacket),
    ] {
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        let error = packetizer
            .push(TtsAudioChunk {
                audio,
                is_last: true,
            })
            .unwrap_err();
        let typed = error.downcast_ref::<TtsPacketizationError>().unwrap();
        assert_eq!(typed.kind, expected_kind);
        assert_eq!(typed.profile, profile);
    }
}

#[test]
fn standard_tts_speex_splits_fixed_quality_7_frames_across_provider_blocks() {
    for (rate, frame_size) in [
        (AudioSampleRate::Hz8000, 38usize),
        (AudioSampleRate::Hz16000, 60usize),
    ] {
        let profile = AudioProfile::new(AudioFormat::Speex, rate);
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        let first_frame = vec![1; frame_size];
        let second_frame = vec![2; frame_size];
        let split = frame_size / 2;
        let mut first_block = first_frame.clone();
        first_block.extend_from_slice(&second_frame[..split]);

        let first = packetizer
            .push(TtsAudioChunk {
                audio: first_block,
                is_last: false,
            })
            .unwrap();
        assert!(first.is_empty());
        assert!(packetizer.buffered_bytes() < frame_size);

        let final_chunks = packetizer
            .push(TtsAudioChunk {
                audio: second_frame[split..].to_vec(),
                is_last: true,
            })
            .unwrap();
        assert_eq!(
            final_chunks,
            vec![
                TtsAudioChunk {
                    audio: first_frame,
                    is_last: false,
                },
                TtsAudioChunk {
                    audio: second_frame,
                    is_last: true,
                },
            ]
        );
    }
}

#[test]
fn standard_tts_speex_final_rejects_a_partial_frame_with_a_typed_error() {
    let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let error = packetizer
        .push(TtsAudioChunk {
            audio: vec![1; 59],
            is_last: true,
        })
        .unwrap_err();
    let typed = error.downcast_ref::<TtsPacketizationError>().unwrap();
    assert_eq!(typed.kind, TtsPacketizationErrorKind::TruncatedPacket);
    assert_eq!(typed.buffered_bytes, 59);
}

#[test]
fn standard_tts_packetizer_keeps_continuous_formats_and_empty_final_lifecycle() {
    for format in [AudioFormat::Mp3, AudioFormat::Pcm] {
        let profile = AudioProfile::new(format, AudioSampleRate::Hz16000);
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        assert_eq!(
            packetizer
                .push(TtsAudioChunk {
                    audio: vec![1, 2, 3],
                    is_last: false,
                })
                .unwrap(),
            vec![TtsAudioChunk {
                audio: vec![1, 2, 3],
                is_last: false,
            }]
        );
        assert_eq!(
            packetizer
                .push(TtsAudioChunk {
                    audio: Vec::new(),
                    is_last: true,
                })
                .unwrap(),
            vec![TtsAudioChunk {
                audio: Vec::new(),
                is_last: true,
            }]
        );
    }

    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let first = packetizer
        .push(TtsAudioChunk {
            audio: vec![0, 2, 1, 2],
            is_last: false,
        })
        .unwrap();
    assert!(first.is_empty());
    let final_chunks = packetizer
        .push(TtsAudioChunk {
            audio: Vec::new(),
            is_last: true,
        })
        .unwrap();
    assert_eq!(
        final_chunks,
        vec![TtsAudioChunk {
            audio: vec![1, 2],
            is_last: true,
        }]
    );
}

#[test]
fn standard_tts_compressed_lookbehind_preserves_order_across_provider_blocks() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();

    let first = packetizer
        .push(TtsAudioChunk {
            audio: vec![0, 1, 1, 0, 1, 2],
            is_last: false,
        })
        .unwrap();
    assert_eq!(
        first,
        vec![TtsAudioChunk {
            audio: vec![1],
            is_last: false,
        }]
    );

    let second = packetizer
        .push(TtsAudioChunk {
            audio: vec![0, 1, 3, 0, 1, 4],
            is_last: false,
        })
        .unwrap();
    assert_eq!(
        second,
        vec![
            TtsAudioChunk {
                audio: vec![2],
                is_last: false,
            },
            TtsAudioChunk {
                audio: vec![3],
                is_last: false,
            },
        ]
    );

    let final_chunks = packetizer
        .push(TtsAudioChunk {
            audio: Vec::new(),
            is_last: true,
        })
        .unwrap();
    assert_eq!(
        final_chunks,
        vec![TtsAudioChunk {
            audio: vec![4],
            is_last: true,
        }]
    );
    assert!(first
        .iter()
        .chain(second.iter())
        .chain(final_chunks.iter())
        .all(|chunk| !chunk.audio.is_empty()));
}

#[test]
fn standard_tts_compressed_empty_final_releases_held_real_packet_for_both_codecs() {
    for (profile, provider_audio, expected_packet) in [
        (
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000),
            [vec![0, 3], vec![7, 8, 9]].concat(),
            vec![7, 8, 9],
        ),
        (
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
            vec![6; 60],
            vec![6; 60],
        ),
    ] {
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        assert!(packetizer
            .push(TtsAudioChunk {
                audio: provider_audio,
                is_last: false,
            })
            .unwrap()
            .is_empty());
        let final_chunks = packetizer
            .push(TtsAudioChunk {
                audio: Vec::new(),
                is_last: true,
            })
            .unwrap();
        assert_eq!(
            final_chunks,
            vec![TtsAudioChunk {
                audio: expected_packet,
                is_last: true,
            }]
        );
    }
}

#[test]
fn standard_tts_compressed_stream_with_no_real_packets_is_a_typed_error() {
    for profile in [
        AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
    ] {
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        let error = packetizer
            .push(TtsAudioChunk {
                audio: Vec::new(),
                is_last: true,
            })
            .unwrap_err();
        let typed = error.downcast_ref::<TtsPacketizationError>().unwrap();
        assert_eq!(typed.kind, TtsPacketizationErrorKind::EmptyCompressedStream);
        assert_eq!(typed.profile, profile);
    }
}

#[test]
fn standard_tts_compressed_final_residual_errors_before_releasing_held_packet() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    assert!(packetizer
        .push(TtsAudioChunk {
            audio: vec![0, 2, 1, 2],
            is_last: false,
        })
        .unwrap()
        .is_empty());

    let error = packetizer
        .push(TtsAudioChunk {
            audio: vec![0],
            is_last: true,
        })
        .unwrap_err();
    let typed = error.downcast_ref::<TtsPacketizationError>().unwrap();
    assert_eq!(typed.kind, TtsPacketizationErrorKind::TruncatedLengthPrefix);
}

#[test]
fn standard_tts_rejects_oversized_provider_block_before_changing_packetizer_state() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let error = packetizer
        .push(TtsAudioChunk {
            audio: vec![0; MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES + 1],
            is_last: false,
        })
        .unwrap_err();
    let typed = error.downcast_ref::<TtsPacketizationError>().unwrap();
    assert_eq!(typed.kind, TtsPacketizationErrorKind::ProviderBlockTooLarge);
    assert_eq!(typed.buffered_bytes, 0);
    assert_eq!(packetizer.buffered_bytes(), 0);
    assert_eq!(packetizer.held_packet_bytes(), 0);

    assert_eq!(
        packetizer
            .push(TtsAudioChunk {
                audio: vec![0, 2, 1, 2],
                is_last: true,
            })
            .unwrap(),
        vec![TtsAudioChunk {
            audio: vec![1, 2],
            is_last: true,
        }]
    );
}

#[test]
fn standard_tts_continuous_formats_keep_large_provider_blocks_unchanged() {
    for format in [AudioFormat::Mp3, AudioFormat::Pcm] {
        let profile = AudioProfile::new(format, AudioSampleRate::Hz16000);
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        let audio = vec![7; MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES + 1];
        assert_eq!(
            packetizer
                .push(TtsAudioChunk {
                    audio: audio.clone(),
                    is_last: true,
                })
                .unwrap(),
            vec![TtsAudioChunk {
                audio,
                is_last: true,
            }]
        );
    }
}

#[test]
fn standard_tts_opus_length_and_finished_state_boundaries_are_typed() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    for declared in [0usize, MAX_STANDARD_OPUS_PACKET_BYTES + 1] {
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        let error = packetizer
            .push(TtsAudioChunk {
                audio: vec![(declared >> 8) as u8, declared as u8],
                is_last: false,
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<TtsPacketizationError>().unwrap().kind,
            TtsPacketizationErrorKind::InvalidPacketLength
        );
    }

    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let mut maximum_packet = vec![
        (MAX_STANDARD_OPUS_PACKET_BYTES >> 8) as u8,
        MAX_STANDARD_OPUS_PACKET_BYTES as u8,
    ];
    maximum_packet.extend(vec![5; MAX_STANDARD_OPUS_PACKET_BYTES]);
    let chunks = packetizer
        .push(TtsAudioChunk {
            audio: maximum_packet,
            is_last: true,
        })
        .unwrap();
    assert_eq!(chunks[0].audio.len(), MAX_STANDARD_OPUS_PACKET_BYTES);
    assert!(chunks[0].is_last);

    let error = packetizer
        .push(TtsAudioChunk {
            audio: Vec::new(),
            is_last: true,
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<TtsPacketizationError>().unwrap().kind,
        TtsPacketizationErrorKind::StreamAlreadyFinished
    );
}

#[test]
fn standard_tts_compressed_retained_batch_never_exceeds_the_64k_output_bound() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let mut held = vec![0, 2];
    held.extend([1, 2]);
    assert!(packetizer
        .push(TtsAudioChunk {
            audio: held,
            is_last: false,
        })
        .unwrap()
        .is_empty());

    let error = packetizer
        .push(TtsAudioChunk {
            audio: vec![0; MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES],
            is_last: false,
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<TtsPacketizationError>().unwrap().kind,
        TtsPacketizationErrorKind::ProviderBlockTooLarge
    );
    assert_eq!(packetizer.buffered_bytes(), 0);
    assert_eq!(packetizer.held_packet_bytes(), 2);
}

#[tokio::test]
async fn standard_tts_compressed_forwarder_rejects_oversized_valid_base64_before_decode() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![0; MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES + 1],
    );
    let message = serde_json::json!({
        "code": 0,
        "data": {"audio": encoded, "status": 1}
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(1);

    let precheck = precheck_standard_tts_provider_audio(
        message["data"]["audio"].as_str().unwrap(),
        &packetizer,
    )
    .unwrap_err();
    assert_eq!(
        precheck
            .downcast_ref::<TtsPacketizationError>()
            .unwrap()
            .kind,
        TtsPacketizationErrorKind::ProviderBlockTooLarge
    );
    let error = forward_standard_tts_audio_frame(&message, &mut packetizer, &tx)
        .await
        .unwrap_err();
    let typed = error.downcast_ref::<TtsPacketizationError>().unwrap();
    assert_eq!(typed.kind, TtsPacketizationErrorKind::ProviderBlockTooLarge);
    assert_eq!(packetizer.buffered_bytes(), 0);
    assert_eq!(packetizer.held_packet_bytes(), 0);
}

#[tokio::test]
async fn standard_tts_compressed_forwarder_accepts_exact_64k_decoded_boundary() {
    let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![3; MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES],
    );
    let message = serde_json::json!({
        "code": 0,
        "data": {"audio": encoded, "status": 1}
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(2_000);

    precheck_standard_tts_provider_audio(message["data"]["audio"].as_str().unwrap(), &packetizer)
        .unwrap();
    assert_eq!(
        forward_standard_tts_audio_frame(&message, &mut packetizer, &tx)
            .await
            .unwrap(),
        Some(false)
    );
    assert_eq!(packetizer.buffered_bytes(), 16);
    assert_eq!(packetizer.held_packet_bytes(), 60);
}

#[tokio::test]
async fn standard_tts_predecode_gate_does_not_limit_continuous_or_mask_invalid_base64() {
    let large_encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![4; MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES + 1],
    );
    let large_message = serde_json::json!({
        "code": 0,
        "data": {"audio": large_encoded, "status": 2}
    });
    let mut mp3 = StandardTtsPacketizer::new(AudioProfile::new(
        AudioFormat::Mp3,
        AudioSampleRate::Hz16000,
    ))
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    precheck_standard_tts_provider_audio(large_message["data"]["audio"].as_str().unwrap(), &mp3)
        .unwrap();
    assert_eq!(
        forward_standard_tts_audio_frame(&large_message, &mut mp3, &tx)
            .await
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        rx.recv().await.unwrap().unwrap().audio.len(),
        MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES + 1
    );

    let invalid_message = serde_json::json!({
        "code": 0,
        "data": {"audio": "!!!!", "status": 2}
    });
    let mut opus = StandardTtsPacketizer::new(AudioProfile::new(
        AudioFormat::Opus,
        AudioSampleRate::Hz16000,
    ))
    .unwrap();
    precheck_standard_tts_provider_audio("!!!!", &opus).unwrap();
    let error = forward_standard_tts_audio_frame(&invalid_message, &mut opus, &tx)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("invalid standard tts audio base64"));
    assert!(error.downcast_ref::<TtsPacketizationError>().is_none());
}

#[tokio::test]
async fn standard_tts_compressed_predecode_gate_rejects_oversized_invalid_base64_by_size() {
    let mut encoded = "A".repeat(MAX_STANDARD_TTS_PROVIDER_BLOCK_BASE64_BYTES);
    encoded.push('!');
    let message = serde_json::json!({
        "code": 0,
        "data": {"audio": encoded, "status": 1}
    });
    let mut packetizer = StandardTtsPacketizer::new(AudioProfile::new(
        AudioFormat::Opus,
        AudioSampleRate::Hz16000,
    ))
    .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(1);

    let error = forward_standard_tts_audio_frame(&message, &mut packetizer, &tx)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<TtsPacketizationError>().unwrap().kind,
        TtsPacketizationErrorKind::ProviderBlockTooLarge
    );
    assert_eq!(packetizer.buffered_bytes(), 0);
    assert_eq!(packetizer.held_packet_bytes(), 0);
}

#[test]
fn standard_tts_compressed_residual_and_held_packet_are_strictly_bounded() {
    let opus = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut opus_packetizer = StandardTtsPacketizer::new(opus).unwrap();
    let mut almost_complete = vec![
        (MAX_STANDARD_OPUS_PACKET_BYTES >> 8) as u8,
        MAX_STANDARD_OPUS_PACKET_BYTES as u8,
    ];
    almost_complete.extend(vec![1; MAX_STANDARD_OPUS_PACKET_BYTES - 1]);
    assert!(opus_packetizer
        .push(TtsAudioChunk {
            audio: almost_complete,
            is_last: false,
        })
        .unwrap()
        .is_empty());
    assert_eq!(
        opus_packetizer.buffered_bytes(),
        2 + MAX_STANDARD_OPUS_PACKET_BYTES - 1
    );
    assert_eq!(opus_packetizer.held_packet_bytes(), 0);
    assert!(opus_packetizer
        .push(TtsAudioChunk {
            audio: vec![2],
            is_last: false,
        })
        .unwrap()
        .is_empty());
    assert_eq!(opus_packetizer.buffered_bytes(), 0);
    assert_eq!(
        opus_packetizer.held_packet_bytes(),
        MAX_STANDARD_OPUS_PACKET_BYTES
    );

    let speex = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let mut speex_packetizer = StandardTtsPacketizer::new(speex).unwrap();
    assert!(speex_packetizer
        .push(TtsAudioChunk {
            audio: vec![3; 119],
            is_last: false,
        })
        .unwrap()
        .is_empty());
    assert_eq!(speex_packetizer.buffered_bytes(), 59);
    assert_eq!(speex_packetizer.held_packet_bytes(), 60);
}

#[tokio::test]
async fn standard_tts_profile_aware_forwarder_publishes_one_raw_opus_packet_per_chunk() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    let provider_block = vec![0, 2, 1, 2, 0, 3, 3, 4, 5];
    let message = serde_json::json!({
        "code": 0,
        "data": {
            "audio": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                provider_block
            ),
            "status": 2
        }
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);

    assert_eq!(
        forward_standard_tts_audio_frame(&message, &mut packetizer, &tx)
            .await
            .unwrap(),
        Some(true)
    );
    drop(tx);
    let first = rx.recv().await.unwrap().unwrap();
    let second = rx.recv().await.unwrap().unwrap();
    assert_eq!(first.audio, vec![1, 2]);
    assert!(!first.is_last);
    assert_eq!(second.audio, vec![3, 4, 5]);
    assert!(second.is_last);
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn standard_tts_compressed_forwarder_treats_final_without_audio_field_as_empty_final() {
    let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
    let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
    packetizer
        .push(TtsAudioChunk {
            audio: vec![0, 2, 1, 2],
            is_last: false,
        })
        .unwrap();
    let message = serde_json::json!({"code": 0, "data": {"status": 2}});
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);

    assert_eq!(
        forward_standard_tts_audio_frame(&message, &mut packetizer, &tx)
            .await
            .unwrap(),
        Some(true)
    );
    drop(tx);
    assert_eq!(
        rx.recv().await.unwrap().unwrap(),
        TtsAudioChunk {
            audio: vec![1, 2],
            is_last: true,
        }
    );
    assert!(rx.recv().await.is_none());
}

#[test]
fn standard_tts_skips_successful_responses_without_audio() {
    for message in [
        serde_json::json!({"code": 0, "data": null}),
        serde_json::json!({"code": 0, "data": {"status": 1}}),
    ] {
        assert_eq!(parse_standard_tts_audio_frame(&message).unwrap(), None);
    }
}

#[test]
fn standard_tts_frame_parser_keeps_audio_and_upstream_errors() {
    let audio = serde_json::json!({
        "code": 0,
        "data": {"audio": "AQID", "status": 2}
    });
    let error = serde_json::json!({
        "code": 11200,
        "message": "invalid request",
        "data": null
    });

    assert_eq!(
        parse_standard_tts_audio_frame(&audio).unwrap(),
        Some(mjy_voice_shop_rs::xfyun::tts::TtsAudioChunk {
            audio: vec![1, 2, 3],
            is_last: true,
        })
    );
    assert!(parse_standard_tts_audio_frame(&error)
        .unwrap_err()
        .to_string()
        .contains("11200"));

    let empty_audio = serde_json::json!({
        "code": 0,
        "data": {"audio": "", "status": 1}
    });
    assert_eq!(
        parse_standard_tts_audio_frame(&empty_audio).unwrap(),
        Some(mjy_voice_shop_rs::xfyun::tts::TtsAudioChunk {
            audio: Vec::new(),
            is_last: false,
        })
    );
}

#[test]
fn tts_parsers_return_typed_errors_and_only_classify_explicit_profiles() {
    let standard_profile = parse_standard_tts_audio_frame(&serde_json::json!({
        "code": 10006,
        "message": "invalid aue",
        "data": null
    }))
    .unwrap_err();
    let private_profile = parse_tts_audio_frame(&serde_json::json!({
        "header": {"code": 10163, "message": "unsupported audio format"}
    }))
    .unwrap_err();

    let standard = standard_profile.downcast_ref::<TtsUpstreamError>().unwrap();
    let private = private_profile.downcast_ref::<TtsUpstreamError>().unwrap();
    assert_eq!(standard.provider, TtsProvider::Standard);
    assert_eq!(standard.code, 10006);
    assert_eq!(standard.kind, TtsUpstreamErrorKind::AudioProfileRejected);
    assert_eq!(private.provider, TtsProvider::SuperSmart);
    assert_eq!(private.kind, TtsUpstreamErrorKind::AudioProfileRejected);
    assert_eq!(
        classify_tts_error(&standard_profile),
        "upstream_audio_profile_rejected"
    );
    assert_eq!(
        classify_tts_error(&private_profile),
        "upstream_audio_profile_rejected"
    );

    for (provider, code, message, expected) in [
        (TtsProvider::Standard, 10006, "invalid vcn", "tts_failed"),
        (TtsProvider::Standard, 10006, "invalid speex", "tts_failed"),
        (
            TtsProvider::Standard,
            10006,
            "invalid sample rate",
            "tts_failed",
        ),
        (
            TtsProvider::Standard,
            10043,
            "audio decoding failed",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::Standard,
            10006,
            "invalid audio rate",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::Standard,
            10007,
            "invalid aue",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::Standard,
            10163,
            "unsupported encoding",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::Standard,
            10163,
            "invalid text parameter",
            "tts_failed",
        ),
        (
            TtsProvider::Standard,
            11200,
            "audio codec license denied",
            "tts_failed",
        ),
        (TtsProvider::SuperSmart, 10006, "invalid vcn", "tts_failed"),
        (
            TtsProvider::SuperSmart,
            10006,
            "invalid speex",
            "tts_failed",
        ),
        (
            TtsProvider::SuperSmart,
            10006,
            "invalid sample rate",
            "tts_failed",
        ),
        (
            TtsProvider::SuperSmart,
            10043,
            "audio decoding failed",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::SuperSmart,
            10006,
            "invalid audio rate",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::SuperSmart,
            10163,
            "unsupported encoding",
            "upstream_audio_profile_rejected",
        ),
        (
            TtsProvider::SuperSmart,
            10163,
            "invalid text parameter",
            "tts_failed",
        ),
        (
            TtsProvider::SuperSmart,
            11200,
            "audio codec license denied",
            "tts_failed",
        ),
    ] {
        let error = match provider {
            TtsProvider::Standard => parse_standard_tts_audio_frame(&serde_json::json!({
                "code": code,
                "message": message,
                "data": null
            }))
            .unwrap_err(),
            TtsProvider::SuperSmart => parse_tts_audio_frame(&serde_json::json!({
                "header": {"code": code, "message": message}
            }))
            .unwrap_err(),
        };
        assert_eq!(
            classify_tts_error(&error),
            expected,
            "{provider:?} {code}: {message}"
        );
        let expected_kind = if expected == "upstream_audio_profile_rejected" {
            TtsUpstreamErrorKind::AudioProfileRejected
        } else {
            TtsUpstreamErrorKind::Other
        };
        assert_eq!(
            error.downcast_ref::<TtsUpstreamError>().unwrap().kind,
            expected_kind,
            "{provider:?} {code}: {message}"
        );
    }

    for provider in [TtsProvider::Standard, TtsProvider::SuperSmart] {
        for (code, message) in [
            (10105, "audio codec auth denied"),
            (11200, "audio codec license expired"),
            (10110, "audio codec quota exhausted"),
            (10200, "audio format server error"),
            (10106, "audio codec QPS exceeded"),
            (10163, "request rate limit exceeded"),
        ] {
            let error = match provider {
                TtsProvider::Standard => parse_standard_tts_audio_frame(&serde_json::json!({
                    "code": code,
                    "message": message,
                    "data": null
                }))
                .unwrap_err(),
                TtsProvider::SuperSmart => parse_tts_audio_frame(&serde_json::json!({
                    "header": {"code": code, "message": message}
                }))
                .unwrap_err(),
            };
            let typed = error.downcast_ref::<TtsUpstreamError>().unwrap();
            assert_eq!(typed.kind, TtsUpstreamErrorKind::Other);
            assert_eq!(
                classify_tts_error(&error),
                "tts_failed",
                "{provider:?} {code}: {message}"
            );
        }
    }
}

#[tokio::test]
async fn standard_tts_consumer_skips_control_frames_and_preserves_chunk_boundaries() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut progress = TtsStreamProgress::default();
    let mut packetizer = StandardTtsPacketizer::new(AudioProfile::new(
        AudioFormat::Mp3,
        AudioSampleRate::Hz16000,
    ))
    .unwrap();
    let frames = [
        serde_json::json!({"code": 0, "data": null}),
        serde_json::json!({"code": 0, "data": {"audio": "AQI=", "status": 1}}),
        serde_json::json!({"code": 0, "data": {"audio": "AwQF", "status": 2}}),
    ];

    assert_eq!(
        forward_standard_tts_audio_frame(&frames[0], &mut packetizer, &tx)
            .await
            .unwrap(),
        None
    );
    let first_is_last = forward_standard_tts_audio_frame(&frames[1], &mut packetizer, &tx)
        .await
        .unwrap()
        .unwrap();
    progress.observe(first_is_last);
    let second_is_last = forward_standard_tts_audio_frame(&frames[2], &mut packetizer, &tx)
        .await
        .unwrap()
        .unwrap();
    progress.observe(second_is_last);
    progress.ensure_complete().unwrap();
    drop(tx);

    assert_eq!(rx.recv().await.unwrap().unwrap().audio, vec![1, 2]);
    assert_eq!(rx.recv().await.unwrap().unwrap().audio, vec![3, 4, 5]);
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn tts_text_io_coupling_cancels_the_other_side_on_any_error() {
    let writer_cancelled = Arc::new(AtomicBool::new(false));
    let writer_signal = writer_cancelled.clone();
    let (writer_started_tx, writer_started_rx) = tokio::sync::oneshot::channel();
    let writer = async move {
        let _guard = CancellationGuard(writer_signal);
        let _ = writer_started_tx.send(());
        std::future::pending::<anyhow::Result<()>>().await
    };
    let reader = async move {
        writer_started_rx.await.unwrap();
        anyhow::bail!("upstream reader failed")
    };

    assert!(couple_tts_text_io(writer, reader).await.is_err());
    assert!(writer_cancelled.load(Ordering::SeqCst));

    let reader_cancelled = Arc::new(AtomicBool::new(false));
    let reader_signal = reader_cancelled.clone();
    let (reader_started_tx, reader_started_rx) = tokio::sync::oneshot::channel();
    let reader = async move {
        let _guard = CancellationGuard(reader_signal);
        let _ = reader_started_tx.send(());
        std::future::pending::<anyhow::Result<()>>().await
    };
    let writer = async move {
        reader_started_rx.await.unwrap();
        anyhow::bail!("text input closed before final status")
    };

    assert!(couple_tts_text_io(writer, reader).await.is_err());
    assert!(reader_cancelled.load(Ordering::SeqCst));

    let writer_cancelled = Arc::new(AtomicBool::new(false));
    let writer_signal = writer_cancelled.clone();
    let (writer_started_tx, writer_started_rx) = tokio::sync::oneshot::channel();
    let writer = async move {
        let _guard = CancellationGuard(writer_signal);
        let _ = writer_started_tx.send(());
        std::future::pending::<anyhow::Result<()>>().await
    };
    let reader = async move {
        writer_started_rx.await.unwrap();
        anyhow::bail!("TTS audio receiver closed")
    };

    assert!(couple_tts_text_io(writer, reader).await.is_err());
    assert!(writer_cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn tts_session_runner_covers_receiver_close_and_timeout_during_pending_phases() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_signal = cancelled.clone();
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(1);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let pending_handshake = async move {
        let _guard = CancellationGuard(cancelled_signal);
        let _ = started_tx.send(());
        std::future::pending::<anyhow::Result<()>>().await
    };
    let close_receiver = tokio::spawn(async move {
        started_rx.await.unwrap();
        drop(audio_rx);
    });

    let closed = run_tts_stream_session(
        &audio_tx,
        std::time::Duration::from_secs(1),
        pending_handshake,
    )
    .await
    .unwrap_err();
    close_receiver.await.unwrap();
    assert!(closed.to_string().contains("receiver closed"));
    assert!(cancelled.load(Ordering::SeqCst));

    let (audio_tx, _audio_rx) = tokio::sync::mpsc::channel(1);
    let timeout = run_tts_stream_session(
        &audio_tx,
        std::time::Duration::from_millis(10),
        std::future::pending::<anyhow::Result<()>>(),
    )
    .await
    .unwrap_err();
    assert!(timeout.to_string().contains("timed out"));
}

#[tokio::test]
async fn standard_and_super_smart_progress_reject_truncated_audio_streams() {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let standard = serde_json::json!({
        "code": 0,
        "data": {"audio": "AQI=", "status": 1}
    });
    let private = serde_json::json!({
        "header": {"code": 0},
        "payload": {"audio": {"audio": "AwQ=", "status": 1}}
    });

    let mut standard_progress = TtsStreamProgress::default();
    let mut standard_packetizer = StandardTtsPacketizer::new(AudioProfile::new(
        AudioFormat::Mp3,
        AudioSampleRate::Hz16000,
    ))
    .unwrap();
    let standard_last = forward_standard_tts_audio_frame(&standard, &mut standard_packetizer, &tx)
        .await
        .unwrap()
        .unwrap();
    standard_progress.observe(standard_last);
    assert!(standard_progress
        .ensure_complete()
        .unwrap_err()
        .to_string()
        .contains("before final frame"));

    let mut private_progress = TtsStreamProgress::default();
    let private_last = forward_tts_audio_frame(&private, &tx)
        .await
        .unwrap()
        .unwrap();
    private_progress.observe(private_last);
    assert!(private_progress
        .ensure_complete()
        .unwrap_err()
        .to_string()
        .contains("before final frame"));
}

#[tokio::test]
async fn empty_final_audio_is_forwarded_and_completes_the_stream() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let frames = [
        serde_json::json!({
            "header": {"code": 0},
            "payload": {"audio": {"audio": "AQI=", "status": 1}}
        }),
        serde_json::json!({
            "header": {"code": 0},
            "payload": {"audio": {"audio": "", "status": 2}}
        }),
    ];
    let mut progress = TtsStreamProgress::default();

    for frame in frames {
        let is_last = forward_tts_audio_frame(&frame, &tx).await.unwrap().unwrap();
        progress.observe(is_last);
    }
    progress.ensure_complete().unwrap();
    drop(tx);

    assert_eq!(rx.recv().await.unwrap().unwrap().audio, vec![1, 2]);
    let final_chunk = rx.recv().await.unwrap().unwrap();
    assert!(final_chunk.audio.is_empty());
    assert!(final_chunk.is_last);
}

#[tokio::test]
async fn streaming_tts_preflight_errors_happen_before_connect() {
    let mut config = AppConfig::default_from_env();
    config.api_key = "key".to_string();
    config.api_secret = "secret".to_string();
    config.tts_endpoint = "ws://127.0.0.1:1/unreachable".to_string();
    config.tts_standard_endpoint = "ws://127.0.0.1:1/unreachable".to_string();
    config.tts_provider = "standard".to_string();

    let profile = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let mut mismatch = stream_audio_profile_chunks(
        config.clone(),
        "你好".to_string(),
        profile,
        TtsProvider::SuperSmart,
    )
    .await;
    let mismatch = tokio::time::timeout(std::time::Duration::from_secs(1), mismatch.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(mismatch.to_string().contains("provider mismatch"));

    let unsupported = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000);
    let mut rejected = stream_audio_profile_chunks(
        config,
        "你好".to_string(),
        unsupported,
        TtsProvider::Standard,
    )
    .await;
    let rejected = tokio::time::timeout(std::time::Duration::from_secs(1), rejected.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        rejected
            .downcast_ref::<mjy_voice_shop_rs::xfyun::tts::TtsAudioProfileError>()
            .unwrap()
            .code(),
        "unsupported_audio_profile"
    );
}

#[tokio::test]
async fn streaming_tts_rejects_empty_text_input_before_connect() {
    let mut config = AppConfig::default_from_env();
    config.api_key = "key".to_string();
    config.api_secret = "secret".to_string();
    config.tts_provider = "super_smart".to_string();
    config.tts_endpoint = "ws://127.0.0.1:1/unreachable".to_string();
    let (text_tx, text_rx) = tokio::sync::mpsc::channel::<TtsTextFrame>(1);
    drop(text_tx);

    let mut audio_rx = stream_super_smart_tts_text_frames_for_profile(
        config,
        text_rx,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    )
    .await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), audio_rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();

    assert!(error.to_string().contains("no text frames"));
}

#[tokio::test]
async fn dropping_audio_receiver_before_first_text_frame_cancels_the_session() {
    let mut config = AppConfig::default_from_env();
    config.api_key = "key".to_string();
    config.api_secret = "secret".to_string();
    config.tts_provider = "super_smart".to_string();
    config.tts_endpoint = "ws://127.0.0.1:1/unreachable".to_string();
    let (text_tx, text_rx) = tokio::sync::mpsc::channel::<TtsTextFrame>(1);

    let audio_rx = stream_super_smart_tts_text_frames_for_profile(
        config,
        text_rx,
        AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
    )
    .await;
    drop(audio_rx);

    tokio::time::timeout(std::time::Duration::from_secs(1), text_tx.closed())
        .await
        .expect("text receiver should be dropped when audio receiver closes");
}

#[test]
fn maps_expected_asr_empty_text_to_user_friendly_message() {
    assert_eq!(
        friendly_error_message("asr_failed", "IAT returned empty text"),
        "没有识别到有效语音，请再说一遍"
    );
    assert!(friendly_error_message("tts_failed", "tts error code: 11200").contains("11200"));
    assert_eq!(
        friendly_error_message("asr_failed", "live IAT session timed out after 20 seconds"),
        "语音识别超时，请再说一遍"
    );
    assert_eq!(LIVE_IAT_SESSION_TIMEOUT, std::time::Duration::from_secs(20));
}

#[test]
fn suppresses_short_empty_asr_without_hiding_long_empty_asr() {
    assert!(should_suppress_empty_asr(620, "IAT returned empty text"));
    assert!(!should_suppress_empty_asr(1400, "IAT returned empty text"));
    assert!(!should_suppress_empty_asr(620, "network timeout"));
}

#[test]
fn matches_multiple_products_with_quantity_and_specs() {
    let products = vec![
        Product::new("cola", "可口可乐", vec!["可乐"], "500ml", 3.5),
        Product::new("water", "怡宝矿泉水", vec!["水", "矿泉水"], "555ml", 2.0),
    ];

    let matches = match_products("我要两瓶可乐和一瓶水", &products);

    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].product_id, "cola");
    assert_eq!(matches[0].quantity, 2);
    assert_eq!(matches[1].product_id, "water");
    assert_eq!(matches[1].quantity, 1);
}

#[test]
fn issues_and_verifies_device_token() {
    let token = issue_device_token("DOLL-0001", "server-secret", 1_800_000_000).unwrap();
    let claims = verify_device_token(&token, "server-secret", 1_700_000_000).unwrap();

    assert_eq!(claims.device_id, "DOLL-0001");
    assert!(verify_device_token(&token, "wrong-secret", 1_700_000_000).is_err());
}

#[test]
fn creates_mock_order_from_matched_products() {
    let products = vec![Product::new("cola", "可口可乐", vec!["可乐"], "500ml", 3.5)];
    let matches = match_products("买两瓶可乐", &products);
    let order = create_mock_order("conv-1", &matches);

    assert!(order.order_id.starts_with("MOCK-"));
    assert_eq!(order.items[0].product_id, "cola");
    assert_eq!(order.items[0].quantity, 2);
    assert_eq!(order.total_amount, 7.0);
}
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

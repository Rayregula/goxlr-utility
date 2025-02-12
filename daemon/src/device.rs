use crate::audio::AudioHandler;
use crate::mic_profile::MicProfileAdapter;
use crate::profile::{version_newer_or_equal_to, ProfileAdapter};
use crate::SettingsHandle;
use anyhow::{anyhow, Result};
use enum_map::EnumMap;
use enumset::EnumSet;
use futures::executor::block_on;
use goxlr_ipc::{DeviceType, FaderStatus, GoXLRCommand, HardwareStatus, MicSettings, MixerStatus};
use goxlr_profile_loader::components::mute::MuteFunction;
use goxlr_profile_loader::SampleButtons;
use goxlr_types::{
    ChannelName, EffectBankPresets, EffectKey, EncoderName, FaderName,
    InputDevice as BasicInputDevice, MicrophoneParamKey, OutputDevice as BasicOutputDevice,
    SampleBank, VersionNumber,
};
use goxlr_usb::buttonstate::{ButtonStates, Buttons};
use goxlr_usb::channelstate::ChannelState::{Muted, Unmuted};
use goxlr_usb::goxlr::GoXLR;
use goxlr_usb::routing::{InputDevice, OutputDevice};
use goxlr_usb::rusb::UsbContext;
use log::{debug, error, info};
use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use strum::IntoEnumIterator;

#[derive(Debug)]
pub struct Device<'a, T: UsbContext> {
    goxlr: GoXLR<T>,
    hardware: HardwareStatus,
    last_buttons: EnumSet<Buttons>,
    button_states: EnumMap<Buttons, ButtonState>,
    profile: ProfileAdapter,
    mic_profile: MicProfileAdapter,
    audio_handler: Option<AudioHandler>,
    settings: &'a SettingsHandle,
}

// Experimental code:
#[derive(Debug, Default, Copy, Clone)]
struct ButtonState {
    press_time: u128,
    hold_handled: bool,
}

impl<'a, T: UsbContext> Device<'a, T> {
    pub fn new(
        goxlr: GoXLR<T>,
        hardware: HardwareStatus,
        profile_name: Option<String>,
        mic_profile_name: Option<String>,
        profile_directory: &Path,
        mic_profile_directory: &Path,
        settings_handle: &'a SettingsHandle,
    ) -> Result<Self> {
        info!(
            "Loading Profile: {}",
            profile_name
                .clone()
                .unwrap_or_else(|| "Not Defined".to_string())
        );
        info!(
            "Loading Mic Profile: {}",
            mic_profile_name
                .clone()
                .unwrap_or_else(|| "Not Defined".to_string())
        );
        let profile = ProfileAdapter::from_named_or_default(profile_name, vec![profile_directory]);
        let mic_profile =
            MicProfileAdapter::from_named_or_default(mic_profile_name, vec![mic_profile_directory]);

        let mut audio_handler = None;
        if let Ok(audio) = AudioHandler::new() {
            audio_handler = Some(audio);
        }

        let mut device = Self {
            profile,
            mic_profile,
            goxlr,
            hardware,
            last_buttons: EnumSet::empty(),
            button_states: EnumMap::default(),
            audio_handler,
            settings: settings_handle,
        };

        device.apply_profile()?;
        device.apply_mic_profile()?;

        Ok(device)
    }

    pub fn serial(&self) -> &str {
        &self.hardware.serial_number
    }

    pub fn status(&self) -> MixerStatus {
        let mut fader_map = [Default::default(); 4];
        fader_map[FaderName::A as usize] = self.get_fader_state(FaderName::A);
        fader_map[FaderName::B as usize] = self.get_fader_state(FaderName::B);
        fader_map[FaderName::C as usize] = self.get_fader_state(FaderName::C);
        fader_map[FaderName::D as usize] = self.get_fader_state(FaderName::D);

        MixerStatus {
            hardware: self.hardware.clone(),
            fader_status: fader_map,
            cough_button: self.profile.get_cough_status(),
            bleep_volume: self.get_bleep_volume(),
            volumes: self.profile.get_volumes(),
            router: self.profile.create_router(),
            router_table: self.profile.create_router_table(),
            mic_status: MicSettings {
                mic_type: self.mic_profile.mic_type(),
                mic_gains: self.mic_profile.mic_gains(),
                noise_gate: self.mic_profile.noise_gate_ipc(),
                equaliser: self.mic_profile.equalizer_ipc(),
                equaliser_mini: self.mic_profile.equalizer_mini_ipc(),
                compressor: self.mic_profile.compressor_ipc(),
            },
            lighting: self
                .profile
                .get_lighting_ipc(self.hardware.device_type == DeviceType::Mini),
            profile_name: self.profile.name().to_owned(),
            mic_profile_name: self.mic_profile.name().to_owned(),
        }
    }

    pub fn profile(&self) -> &ProfileAdapter {
        &self.profile
    }

    pub fn mic_profile(&self) -> &MicProfileAdapter {
        &self.mic_profile
    }

    pub async fn monitor_inputs(&mut self) -> Result<()> {
        self.hardware.usb_device.has_kernel_driver_attached =
            self.goxlr.usb_device_has_kernel_driver_active()?;

        // Let the audio handle handle stuff..
        if let Some(audio_handler) = &mut self.audio_handler {
            audio_handler.check_playing();
            self.sync_sample_lighting().await?;
        }

        if let Ok(state) = self.goxlr.get_button_states() {
            self.update_volumes_to(state.volumes);
            self.update_encoders_to(state.encoders)?;

            let pressed_buttons = state.pressed.difference(self.last_buttons);
            for button in pressed_buttons {
                // This is a new press, store it in the states..
                self.button_states[button] = ButtonState {
                    press_time: self.get_epoch_ms(),
                    hold_handled: false,
                };

                if let Err(error) = self.on_button_down(button).await {
                    error!("{}", error);
                }
            }

            let released_buttons = self.last_buttons.difference(state.pressed);
            for button in released_buttons {
                let button_state = self.button_states[button];

                // Output errors, but don't throw them up the stack!
                if let Err(error) = self.on_button_up(button, &button_state).await {
                    error!("{}", error);
                }

                self.button_states[button] = ButtonState {
                    press_time: 0,
                    hold_handled: false,
                }
            }

            // Finally, iterate over our existing button states, and see if any have been
            // pressed for more than half a second and not handled.
            for button in state.pressed {
                if !self.button_states[button].hold_handled {
                    let now = self.get_epoch_ms();
                    if (now - self.button_states[button].press_time) > 500 {
                        if let Err(error) = self.on_button_hold(button).await {
                            error!("{}", error);
                        }
                        self.button_states[button].hold_handled = true;
                    }
                }
            }

            self.last_buttons = state.pressed;
        }

        Ok(())
    }

    async fn on_button_down(&mut self, button: Buttons) -> Result<()> {
        debug!("Handling Button Down: {:?}", button);

        match button {
            Buttons::MicrophoneMute => {
                self.handle_cough_mute(true, false, false, false).await?;
            }
            Buttons::Bleep => {
                self.handle_swear_button(true).await?;
            }
            _ => {}
        }
        self.update_button_states()?;
        Ok(())
    }

    async fn on_button_hold(&mut self, button: Buttons) -> Result<()> {
        debug!("Handling Button Hold: {:?}", button);
        match button {
            Buttons::Fader1Mute => {
                self.handle_fader_mute(FaderName::A, true).await?;
            }
            Buttons::Fader2Mute => {
                self.handle_fader_mute(FaderName::B, true).await?;
            }
            Buttons::Fader3Mute => {
                self.handle_fader_mute(FaderName::C, true).await?;
            }
            Buttons::Fader4Mute => {
                self.handle_fader_mute(FaderName::D, true).await?;
            }
            Buttons::MicrophoneMute => {
                self.handle_cough_mute(false, false, true, false).await?;
            }
            _ => {}
        }
        self.update_button_states()?;
        Ok(())
    }

    async fn on_button_up(&mut self, button: Buttons, state: &ButtonState) -> Result<()> {
        debug!(
            "Handling Button Release: {:?}, Has Long Press Handled: {:?}",
            button, state.hold_handled
        );
        match button {
            Buttons::Fader1Mute => {
                if !state.hold_handled {
                    self.handle_fader_mute(FaderName::A, false).await?;
                }
            }
            Buttons::Fader2Mute => {
                if !state.hold_handled {
                    self.handle_fader_mute(FaderName::B, false).await?;
                }
            }
            Buttons::Fader3Mute => {
                if !state.hold_handled {
                    self.handle_fader_mute(FaderName::C, false).await?;
                }
            }
            Buttons::Fader4Mute => {
                if !state.hold_handled {
                    self.handle_fader_mute(FaderName::D, false).await?;
                }
            }
            Buttons::MicrophoneMute => {
                self.handle_cough_mute(false, true, false, state.hold_handled)
                    .await?;
            }
            Buttons::Bleep => {
                self.handle_swear_button(false).await?;
            }
            Buttons::EffectSelect1 => {
                self.load_effect_bank(EffectBankPresets::Preset1).await?;
            }
            Buttons::EffectSelect2 => {
                self.load_effect_bank(EffectBankPresets::Preset2).await?;
            }
            Buttons::EffectSelect3 => {
                self.load_effect_bank(EffectBankPresets::Preset3).await?;
            }
            Buttons::EffectSelect4 => {
                self.load_effect_bank(EffectBankPresets::Preset4).await?;
            }
            Buttons::EffectSelect5 => {
                self.load_effect_bank(EffectBankPresets::Preset5).await?;
            }
            Buttons::EffectSelect6 => {
                self.load_effect_bank(EffectBankPresets::Preset6).await?;
            }

            // The following 3 are simple, but will need more work once effects are
            // actually applied!
            Buttons::EffectMegaphone => {
                self.toggle_megaphone().await?;
            }
            Buttons::EffectRobot => {
                self.toggle_robot().await?;
            }
            Buttons::EffectHardTune => {
                self.toggle_hardtune().await?;
            }
            Buttons::EffectFx => {
                self.toggle_effects().await?;
            }

            Buttons::SamplerSelectA => {
                self.load_sample_bank(SampleBank::A).await?;
                self.load_colour_map()?;
            }
            Buttons::SamplerSelectB => {
                self.load_sample_bank(SampleBank::B).await?;
                self.load_colour_map()?;
            }
            Buttons::SamplerSelectC => {
                self.load_sample_bank(SampleBank::C).await?;
                self.load_colour_map()?;
            }

            Buttons::SamplerBottomLeft => {
                self.handle_sample_button(SampleButtons::BottomLeft).await?;
            }
            Buttons::SamplerBottomRight => {
                self.handle_sample_button(SampleButtons::BottomRight)
                    .await?;
            }
            Buttons::SamplerTopLeft => {
                self.handle_sample_button(SampleButtons::TopLeft).await?;
            }
            Buttons::SamplerTopRight => {
                self.handle_sample_button(SampleButtons::TopRight).await?;
            }
            _ => {}
        }
        self.update_button_states()?;
        Ok(())
    }

    async fn handle_fader_mute(&mut self, fader: FaderName, held: bool) -> Result<()> {
        // OK, so a fader button has been pressed, we need to determine behaviour, based on the colour map..
        let channel = self.profile.get_fader_assignment(fader);
        let current_volume = self.profile.get_channel_volume(channel);

        let (muted_to_x, muted_to_all, mute_function) = self.profile.get_mute_button_state(fader);

        // Map the channel to BasicInputDevice in case we need it later..
        let basic_input = match channel {
            ChannelName::Mic => Some(BasicInputDevice::Microphone),
            ChannelName::LineIn => Some(BasicInputDevice::LineIn),
            ChannelName::Console => Some(BasicInputDevice::Console),
            ChannelName::System => Some(BasicInputDevice::System),
            ChannelName::Game => Some(BasicInputDevice::Game),
            ChannelName::Chat => Some(BasicInputDevice::Chat),
            ChannelName::Sample => Some(BasicInputDevice::Samples),
            ChannelName::Music => Some(BasicInputDevice::Music),
            _ => None,
        };

        // Should we be muting this fader to all channels?
        if held || (!muted_to_x && mute_function == MuteFunction::All) {
            if held && muted_to_all {
                // Holding the button when it's already muted to all does nothing.
                return Ok(());
            }

            self.profile
                .set_mute_button_previous_volume(fader, current_volume);

            self.goxlr.set_volume(channel, 0)?;
            self.goxlr.set_channel_state(channel, Muted)?;

            self.profile.set_mute_button_on(fader, true);

            if held {
                self.profile.set_mute_button_blink(fader, true);
            }

            self.profile.set_channel_volume(channel, 0);

            return Ok(());
        }

        // Button has been pressed, and we're already in some kind of muted state..
        if !held && muted_to_x {
            // Disable the lighting regardless of action
            self.profile.set_mute_button_on(fader, false);
            self.profile.set_mute_button_blink(fader, false);

            if muted_to_all || mute_function == MuteFunction::All {
                let previous_volume = self.profile.get_mute_button_previous_volume(fader);

                self.goxlr.set_volume(channel, previous_volume)?;
                self.profile.set_channel_volume(channel, previous_volume);

                if channel != ChannelName::Mic
                    || (channel == ChannelName::Mic && !self.mic_muted_by_cough())
                {
                    self.goxlr.set_channel_state(channel, Unmuted)?;
                }
            } else if basic_input.is_some() {
                self.apply_routing(basic_input.unwrap())?;
            }

            return Ok(());
        }

        if !held && !muted_to_x && mute_function != MuteFunction::All {
            // Mute channel to X via transient routing table update
            self.profile.set_mute_button_on(fader, true);
            if basic_input.is_some() {
                self.apply_routing(basic_input.unwrap())?;
            }
        }
        Ok(())
    }

    async fn unmute_if_muted(&mut self, fader: FaderName) -> Result<()> {
        let (muted_to_x, muted_to_all, _mute_function) = self.profile.get_mute_button_state(fader);

        if muted_to_x || muted_to_all {
            self.handle_fader_mute(fader, false).await?;
        }

        Ok(())
    }

    async fn unmute_chat_if_muted(&mut self) -> Result<()> {
        let (_mute_toggle, muted_to_x, muted_to_all, _mute_function) =
            self.profile.get_mute_chat_button_state();

        if muted_to_x || muted_to_all {
            self.handle_cough_mute(true, false, false, false).await?;
        }

        Ok(())
    }

    // This one's a little obnoxious because it's heavily settings dependent, so will contain a
    // large volume of comments working through states, feel free to remove them later :)
    async fn handle_cough_mute(
        &mut self,
        press: bool,
        release: bool,
        held: bool,
        held_called: bool,
    ) -> Result<()> {
        // This *GENERALLY* works in the same way as other mute buttons, however we need to
        // accommodate the hold and toggle behaviours, so lets grab the config.
        let (mute_toggle, muted_to_x, muted_to_all, mute_function) =
            self.profile.get_mute_chat_button_state();

        // Ok, lets handle things in order, was this button just pressed?
        if press {
            if mute_toggle {
                // Mute toggles are only handled on release.
                return Ok(());
            }

            // Enable the cough button in all cases..
            self.profile.set_mute_chat_button_on(true);

            if mute_function == MuteFunction::All {
                // In this scenario, we should just set cough_button_on and mute the channel.
                self.goxlr.set_channel_state(ChannelName::Mic, Muted)?;
                return Ok(());
            }

            self.apply_routing(BasicInputDevice::Microphone)?;
            return Ok(());
        }

        if held {
            if !mute_toggle {
                // Holding in this scenario just keeps the channel muted, so no change here.
                return Ok(());
            }

            // We're togglable, so enable blink, set cough_button_on, mute the channel fully and
            // remove any transient routing which may be set.
            self.profile.set_mute_chat_button_on(true);
            self.profile.set_mute_chat_button_blink(true);

            self.goxlr.set_channel_state(ChannelName::Mic, Muted)?;
            self.apply_routing(BasicInputDevice::Microphone)?;
            return Ok(());
        }

        if release {
            if mute_toggle {
                if held_called {
                    // We don't need to do anything here, a long press has already been handled.
                    return Ok(());
                }

                if muted_to_x || muted_to_all {
                    self.profile.set_mute_chat_button_on(false);
                    self.profile.set_mute_chat_button_blink(false);

                    if (muted_to_all || (muted_to_x && mute_function == MuteFunction::All))
                        && !self.mic_muted_by_fader()
                    {
                        self.goxlr.set_channel_state(ChannelName::Mic, Unmuted)?;
                    }

                    if muted_to_x && mute_function != MuteFunction::All {
                        self.apply_routing(BasicInputDevice::Microphone)?;
                    }

                    return Ok(());
                }

                // In all cases, enable the button
                self.profile.set_mute_chat_button_on(true);

                if mute_function == MuteFunction::All {
                    self.goxlr.set_channel_state(ChannelName::Mic, Muted)?;
                    return Ok(());
                }

                // Update the transient routing..
                self.apply_routing(BasicInputDevice::Microphone)?;
                return Ok(());
            }

            self.profile.set_mute_chat_button_on(false);
            if mute_function == MuteFunction::All {
                if !self.mic_muted_by_fader() {
                    self.goxlr.set_channel_state(ChannelName::Chat, Unmuted)?;
                }
                return Ok(());
            }

            // Disable button and refresh transient routing
            self.apply_routing(BasicInputDevice::Microphone)?;
            return Ok(());
        }

        Ok(())
    }

    async fn handle_swear_button(&mut self, press: bool) -> Result<()> {
        // Pretty simple, turn the light on when pressed, off when released..
        self.profile.set_swear_button_on(press);
        Ok(())
    }

    async fn load_sample_bank(&mut self, bank: SampleBank) -> Result<()> {
        self.profile.load_sample_bank(bank);

        Ok(())
    }

    // This currently only gets called on release, this will change.
    async fn handle_sample_button(&mut self, button: SampleButtons) -> Result<()> {
        if self.audio_handler.is_none() {
            return Err(anyhow!(
                "Not handling button, audio handler not configured."
            ));
        }

        if !self.profile.current_sample_bank_has_samples(button) {
            // On release, so do nothing really..
            return Ok(());
        }

        let sample = self.profile.get_sample_file(button);
        let mut sample_path = self.settings.get_samples_directory().await;

        if sample.starts_with("Recording_") {
            sample_path = sample_path.join("Recorded");
        }

        sample_path = sample_path.join(sample);

        if !sample_path.exists() {
            return Err(anyhow!("Sample File does not exist!"));
        }

        debug!("Attempting to play: {}", sample_path.to_string_lossy());
        let audio_handler = self.audio_handler.as_mut().unwrap();
        audio_handler.play_for_button(button, sample_path.to_str().unwrap().to_string())?;
        self.profile.set_sample_button_state(button, true);

        Ok(())
    }

    async fn sync_sample_lighting(&mut self) -> Result<()> {
        if self.audio_handler.is_none() {
            // No audio handler, no point.
            return Ok(());
        }

        let mut changed = false;

        for button in SampleButtons::iter() {
            let playing = self
                .audio_handler
                .as_ref()
                .unwrap()
                .is_sample_playing(button);

            if self.profile.is_sample_active(button) && !playing {
                self.profile.set_sample_button_state(button, false);
                changed = true;
            }
        }

        if changed {
            self.update_button_states()?;
        }

        Ok(())
    }

    async fn load_effect_bank(&mut self, preset: EffectBankPresets) -> Result<()> {
        self.profile.load_effect_bank(preset);
        self.load_effects()?;
        self.set_pitch_mode()?;

        // Configure the various parts..
        let mut keyset = HashSet::new();
        keyset.extend(self.mic_profile.get_reverb_keyset());
        keyset.extend(self.mic_profile.get_echo_keyset());
        keyset.extend(self.mic_profile.get_pitch_keyset());
        keyset.extend(self.mic_profile.get_gender_keyset());
        keyset.extend(self.mic_profile.get_megaphone_keyset());
        keyset.extend(self.mic_profile.get_robot_keyset());
        keyset.extend(self.mic_profile.get_hardtune_keyset());

        self.apply_effects(keyset)?;

        Ok(())
    }

    async fn toggle_megaphone(&mut self) -> Result<()> {
        self.profile.toggle_megaphone();
        self.apply_effects(HashSet::from([EffectKey::MegaphoneEnabled]))?;
        Ok(())
    }

    async fn toggle_robot(&mut self) -> Result<()> {
        self.profile.toggle_robot();
        self.apply_effects(HashSet::from([EffectKey::RobotEnabled]))?;
        Ok(())
    }

    async fn toggle_hardtune(&mut self) -> Result<()> {
        self.profile.toggle_hardtune();
        self.apply_effects(HashSet::from([EffectKey::HardTuneEnabled]))?;
        self.set_pitch_mode()?;
        Ok(())
    }

    async fn toggle_effects(&mut self) -> Result<()> {
        self.profile.toggle_effects();

        // When this changes, we need to update all the 'Enabled' keys..
        let mut key_updates = HashSet::new();
        key_updates.insert(EffectKey::Encoder1Enabled);
        key_updates.insert(EffectKey::Encoder2Enabled);
        key_updates.insert(EffectKey::Encoder3Enabled);
        key_updates.insert(EffectKey::Encoder4Enabled);

        key_updates.insert(EffectKey::MegaphoneEnabled);
        key_updates.insert(EffectKey::HardTuneEnabled);
        key_updates.insert(EffectKey::RobotEnabled);
        self.apply_effects(key_updates)?;

        Ok(())
    }

    fn mic_muted_by_fader(&self) -> bool {
        // Is the mute button even assigned to a fader?
        let mic_fader_id = self.profile.get_mic_fader_id();

        if mic_fader_id == 4 {
            return false;
        }

        let fader = self.profile.fader_from_id(mic_fader_id);
        let (muted_to_x, muted_to_all, mute_function) = self.profile.get_mute_button_state(fader);

        muted_to_all || (muted_to_x && mute_function == MuteFunction::All)
    }

    fn mic_muted_by_cough(&self) -> bool {
        let (_mute_toggle, muted_to_x, muted_to_all, mute_function) =
            self.profile.get_mute_chat_button_state();

        muted_to_all || (muted_to_x && mute_function == MuteFunction::All)
    }

    fn update_volumes_to(&mut self, volumes: [u8; 4]) {
        for fader in FaderName::iter() {
            let channel = self.profile.get_fader_assignment(fader);
            let old_volume = self.profile.get_channel_volume(channel);

            let new_volume = volumes[fader as usize];
            if new_volume != old_volume {
                debug!(
                    "Updating {} volume from {} to {} as a human moved the fader",
                    channel, old_volume, new_volume
                );
                self.profile.set_channel_volume(channel, new_volume);
            }
        }
    }

    fn update_encoders_to(&mut self, encoders: [i8; 4]) -> Result<()> {
        // Ok, this is funky, due to the way pitch works, the encoder 'value' doesn't match
        // the profile value if hardtune is enabled, so we'll pre-emptively calculate pitch here..
        let mut pitch_value = encoders[0];
        if self.profile.is_hardtune_pitch_enabled() {
            pitch_value *= 12;
        } else if self.profile.is_pitch_narrow() {
            pitch_value /= 2;
        }

        if pitch_value != self.profile.get_pitch_value() {
            debug!(
                "Updating PITCH value from {} to {} as human moved the dial",
                self.profile.get_pitch_value(),
                pitch_value
            );

            // Ok, if hard tune is enabled, multiply this value by 12..
            self.profile.set_pitch_value(pitch_value);
            self.apply_effects(HashSet::from([EffectKey::PitchAmount]))?;
        }

        if encoders[1] != self.profile.get_gender_value() {
            debug!(
                "Updating GENDER value from {} to {} as human moved the dial",
                self.profile.get_gender_value(),
                encoders[1]
            );
            self.profile.set_gender_value(encoders[1]);
            self.apply_effects(HashSet::from([EffectKey::GenderAmount]))?;
        }

        if encoders[2] != self.profile.get_reverb_value() {
            debug!(
                "Updating REVERB value from {} to {} as human moved the dial",
                self.profile.get_reverb_value(),
                encoders[2]
            );
            self.profile.set_reverb_value(encoders[2]);
            self.apply_effects(HashSet::from([EffectKey::ReverbAmount]))?;
        }

        if encoders[3] != self.profile.get_echo_value() {
            debug!(
                "Updating ECHO value from {} to {} as human moved the dial",
                self.profile.get_echo_value(),
                encoders[3]
            );
            self.profile.set_echo_value(encoders[3]);
            self.apply_effects(HashSet::from([EffectKey::EchoAmount]))?;
        }

        Ok(())
    }

    pub async fn perform_command(&mut self, command: GoXLRCommand) -> Result<()> {
        match command {
            GoXLRCommand::SetFader(fader, channel) => {
                self.set_fader(fader, channel).await?;
            }
            GoXLRCommand::SetFaderMuteFunction(fader, behaviour) => {
                if self.profile.get_mute_button_behaviour(fader) == behaviour {
                    // Settings are the same..
                    return Ok(());
                }

                // Unmute the channel to prevent weirdness, then set new behaviour
                self.unmute_if_muted(fader).await?;
                self.profile.set_mute_button_behaviour(fader, behaviour);
            }

            GoXLRCommand::SetVolume(channel, volume) => {
                self.goxlr.set_volume(channel, volume)?;
                self.profile.set_channel_volume(channel, volume);
            }

            GoXLRCommand::SetCoughMuteFunction(mute_function) => {
                if self.profile.get_chat_mute_button_behaviour() == mute_function {
                    // Settings are the same..
                    return Ok(());
                }

                // Unmute the channel to prevent weirdness, then set new behaviour
                self.unmute_chat_if_muted().await?;
                self.profile.set_chat_mute_button_behaviour(mute_function);
            }
            GoXLRCommand::SetCoughIsHold(is_hold) => {
                self.unmute_chat_if_muted().await?;
                self.profile.set_chat_mute_button_is_held(is_hold);
            }
            GoXLRCommand::SetSwearButtonVolume(volume) => {
                if volume < -34 || volume > 0 {
                    return Err(anyhow!("Mute volume must be between -34 and 0"));
                }
                self.settings
                    .set_device_bleep_volume(self.serial(), volume)
                    .await;
                self.settings.save().await;

                self.goxlr
                    .set_effect_values(&[(EffectKey::BleepLevel, volume as i32)])?;
            }
            GoXLRCommand::SetMicrophoneType(mic_type) => {
                self.mic_profile.set_mic_type(mic_type);
                self.apply_mic_gain()?;
            }
            GoXLRCommand::SetMicrophoneGain(mic_type, gain) => {
                self.mic_profile.set_mic_type(mic_type);
                self.mic_profile.set_mic_gain(mic_type, gain);
                self.apply_mic_gain()?;
            }
            GoXLRCommand::SetRouter(input, output, enabled) => {
                debug!("Setting Routing: {:?} {:?} {}", input, output, enabled);
                self.profile.set_routing(input, output, enabled);

                // Apply the change..
                self.apply_routing(input)?;
            }

            // Equaliser
            GoXLRCommand::SetEqMiniGain(gain, value) => {
                if value < -9 || value > 9 {
                    return Err(anyhow!("Gain volume should be between -9 and 9 dB"));
                }

                let param = self.mic_profile.set_mini_eq_gain(gain, value);
                self.apply_mic_params(HashSet::from([param]))?;
            }
            GoXLRCommand::SetEqMiniFreq(freq, value) => {
                // TODO: Verify?
                if !(300.0..=18000.0).contains(&value) {
                    return Err(anyhow!("EQ Frequency should be between 300hz and 18khz"));
                }

                let param = self.mic_profile.set_mini_eq_freq(freq, value);
                self.apply_mic_params(HashSet::from([param]))?;
            }
            GoXLRCommand::SetEqGain(gain, value) => {
                if value < -9 || value > 9 {
                    return Err(anyhow!("Gain volume should be between -9 and 9 dB"));
                }

                let param = self.mic_profile.set_eq_gain(gain, value);
                self.apply_effects(HashSet::from([param]))?;
            }
            GoXLRCommand::SetEqFreq(freq, value) => {
                let param = self.mic_profile.set_eq_freq(freq, value)?;
                self.apply_effects(HashSet::from([param]))?;
            }
            GoXLRCommand::SetGateThreshold(value) => {
                if value > 0 || value < -59 {
                    return Err(anyhow!("Threshold should be between 0 and -59dB"));
                }
                self.mic_profile.set_gate_threshold(value);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::GateThreshold]))?;
                self.apply_effects(HashSet::from([EffectKey::GateThreshold]))?;
            }

            // Noise Gate
            GoXLRCommand::SetGateAttenuation(percentage) => {
                if percentage > 100 {
                    return Err(anyhow!("Attentuation should be a percentage"));
                }
                self.mic_profile.set_gate_attenuation(percentage);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::GateAttenuation]))?;
                self.apply_effects(HashSet::from([EffectKey::GateAttenuation]))?;
            }
            GoXLRCommand::SetGateAttack(attack_time) => {
                self.mic_profile.set_gate_attack(attack_time);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::GateAttack]))?;
                self.apply_effects(HashSet::from([EffectKey::GateAttack]))?;
            }
            GoXLRCommand::SetGateRelease(release_time) => {
                self.mic_profile.set_gate_release(release_time);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::GateRelease]))?;
                self.apply_effects(HashSet::from([EffectKey::GateRelease]))?;
            }
            GoXLRCommand::SetGateActive(active) => {
                self.mic_profile.set_gate_active(active);

                // GateEnabled appears to only be an effect key.
                self.apply_effects(HashSet::from([EffectKey::GateEnabled]))?;
            }

            // Compressor
            GoXLRCommand::SetCompressorThreshold(value) => {
                if value > 0 || value < -24 {
                    return Err(anyhow!("Compressor Threshold must be between 0 and -24 dB"));
                }
                self.mic_profile.set_compressor_threshold(value);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::CompressorMakeUpGain]))?;
                self.apply_effects(HashSet::from([EffectKey::CompressorMakeUpGain]))?;
            }
            GoXLRCommand::SetCompressorRatio(ratio) => {
                self.mic_profile.set_compressor_ratio(ratio);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::CompressorRatio]))?;
                self.apply_effects(HashSet::from([EffectKey::CompressorRatio]))?;
            }
            GoXLRCommand::SetCompressorAttack(value) => {
                self.mic_profile.set_compressor_attack(value);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::CompressorAttack]))?;
                self.apply_effects(HashSet::from([EffectKey::CompressorAttack]))?;
            }
            GoXLRCommand::SetCompressorReleaseTime(value) => {
                self.mic_profile.set_compressor_release(value);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::CompressorRelease]))?;
                self.apply_effects(HashSet::from([EffectKey::CompressorRelease]))?;
            }
            GoXLRCommand::SetCompressorMakeupGain(value) => {
                if value > 24 {
                    return Err(anyhow!("Makeup Gain should be between 0 and 24dB"));
                }
                self.mic_profile.set_compressor_makeup(value);
                self.apply_mic_params(HashSet::from([MicrophoneParamKey::CompressorMakeUpGain]))?;
                self.apply_effects(HashSet::from([EffectKey::CompressorMakeUpGain]))?;
            }

            // Colouring..
            GoXLRCommand::SetFaderDisplayStyle(fader, display) => {
                self.profile.set_fader_display(fader, display);
                self.set_fader_display_from_profile(fader)?;
            }
            GoXLRCommand::SetFaderColours(fader, top, bottom) => {
                // Need to get the fader colour map, and set values..
                self.profile.set_fader_colours(fader, top, bottom)?;
                self.load_colour_map()?;
            }
            GoXLRCommand::SetAllFaderColours(top, bottom) => {
                // I considered this as part of SetFaderColours, but spamming a new colour map
                // for every fader change seemed excessive, this allows us to set them all before
                // reloading.
                for fader in FaderName::iter() {
                    self.profile
                        .set_fader_colours(fader, top.to_owned(), bottom.to_owned())?;
                }
                self.load_colour_map()?;
            }
            GoXLRCommand::SetAllFaderDisplayStyle(display_style) => {
                for fader in FaderName::iter() {
                    self.profile.set_fader_display(fader, display_style);
                }
                self.load_colour_map()?;
            }
            GoXLRCommand::SetButtonColours(target, colour, colour2) => {
                self.profile
                    .set_button_colours(target, colour, colour2.as_ref())?;

                // Reload the colour map and button states..
                self.load_colour_map()?;
                self.update_button_states()?;
            }
            GoXLRCommand::SetButtonOffStyle(target, off_style) => {
                self.profile.set_button_off_style(target, off_style);

                self.load_colour_map()?;
                self.update_button_states()?;
            }
            GoXLRCommand::SetButtonGroupColours(target, colour, colour_2) => {
                self.profile
                    .set_group_button_colours(target, colour, colour_2)?;

                self.load_colour_map()?;
                self.update_button_states()?;
            }
            GoXLRCommand::SetButtonGroupOffStyle(target, off_style) => {
                self.profile.set_group_button_off_style(target, off_style);
                self.load_colour_map()?;
                self.update_button_states()?;
            }

            // Profiles
            GoXLRCommand::LoadProfile(profile_name) => {
                let profile_directory = self.settings.get_profile_directory().await;
                self.profile = ProfileAdapter::from_named(profile_name, vec![&profile_directory])?;
                self.apply_profile()?;
                self.settings
                    .set_device_profile_name(self.serial(), self.profile.name())
                    .await;
                self.settings.save().await;
            }
            GoXLRCommand::SaveProfile() => {
                let profile_directory = self.settings.get_profile_directory().await;
                let profile_name = self.settings.get_device_profile_name(self.serial()).await;

                if let Some(profile_name) = profile_name {
                    self.profile
                        .write_profile(profile_name, &profile_directory, true)?;
                }
            }
            GoXLRCommand::SaveProfileAs(profile_name) => {
                let profile_directory = self.settings.get_profile_directory().await;
                self.profile
                    .write_profile(profile_name.clone(), &profile_directory, false)?;

                // Save the new name in the settings
                self.settings
                    .set_device_profile_name(self.serial(), profile_name.as_str())
                    .await;

                self.settings.save().await;
            }
            GoXLRCommand::LoadMicProfile(mic_profile_name) => {
                let mic_profile_directory = self.settings.get_mic_profile_directory().await;
                self.mic_profile =
                    MicProfileAdapter::from_named(mic_profile_name, vec![&mic_profile_directory])?;
                self.apply_mic_profile()?;
                self.settings
                    .set_device_mic_profile_name(self.serial(), self.mic_profile.name())
                    .await;
                self.settings.save().await;
            }
            GoXLRCommand::SaveMicProfile() => {
                let mic_profile_directory = self.settings.get_mic_profile_directory().await;
                let mic_profile_name = self
                    .settings
                    .get_device_mic_profile_name(self.serial())
                    .await;

                if let Some(profile_name) = mic_profile_name {
                    self.mic_profile
                        .write_profile(profile_name, &mic_profile_directory, true)?;
                }
            }
            GoXLRCommand::SaveMicProfileAs(profile_name) => {
                let profile_directory = self.settings.get_mic_profile_directory().await;
                self.mic_profile
                    .write_profile(profile_name.clone(), &profile_directory, false)?;

                // Save the new name in the settings
                self.settings
                    .set_device_mic_profile_name(self.serial(), profile_name.as_str())
                    .await;

                self.settings.save().await;
            }
        }

        Ok(())
    }

    fn update_button_states(&mut self) -> Result<()> {
        let button_states = self.create_button_states();
        self.goxlr.set_button_states(button_states)?;
        Ok(())
    }

    fn create_button_states(&self) -> [ButtonStates; 24] {
        let mut result = [ButtonStates::DimmedColour1; 24];

        for button in Buttons::iter() {
            result[button as usize] = self.profile.get_button_colour_state(button);
        }

        // Replace the Cough Button button data with correct data.
        result[Buttons::MicrophoneMute as usize] = self.profile.get_mute_chat_button_colour_state();
        result
    }

    // This applies routing for a single input channel..
    fn apply_channel_routing(
        &mut self,
        input: BasicInputDevice,
        router: EnumMap<BasicOutputDevice, bool>,
    ) -> Result<()> {
        let (left_input, right_input) = InputDevice::from_basic(&input);
        let mut left = [0; 22];
        let mut right = [0; 22];

        for output in BasicOutputDevice::iter() {
            if router[output] {
                let (left_output, right_output) = OutputDevice::from_basic(&output);

                left[left_output.position()] = 0x20;
                right[right_output.position()] = 0x20;
            }
        }

        // We need to handle hardtune configuration here as well..
        let hardtune_position = OutputDevice::HardTune.position();
        if self.profile.is_active_hardtune_source_all() {
            match input {
                BasicInputDevice::Music
                | BasicInputDevice::Game
                | BasicInputDevice::LineIn
                | BasicInputDevice::System => {
                    left[hardtune_position] = 0x04;
                    right[hardtune_position] = 0x04;
                }
                _ => {}
            }
        } else {
            // We need to match only against a specific target..
            if input == self.profile.get_active_hardtune_source() {
                left[hardtune_position] = 0x10;
                right[hardtune_position] = 0x10;
            }
        }

        self.goxlr.set_routing(left_input, left)?;
        self.goxlr.set_routing(right_input, right)?;

        Ok(())
    }

    fn apply_transient_routing(
        &self,
        input: BasicInputDevice,
        router: &mut EnumMap<BasicOutputDevice, bool>,
    ) {
        // Not all channels are routable, so map the inputs to channels before checking..
        let channel_name = match input {
            BasicInputDevice::Microphone => ChannelName::Mic,
            BasicInputDevice::Chat => ChannelName::Chat,
            BasicInputDevice::Music => ChannelName::Music,
            BasicInputDevice::Game => ChannelName::Game,
            BasicInputDevice::Console => ChannelName::Console,
            BasicInputDevice::LineIn => ChannelName::LineIn,
            BasicInputDevice::System => ChannelName::System,
            BasicInputDevice::Samples => ChannelName::Sample,
        };

        for fader in FaderName::iter() {
            if self.profile.get_fader_assignment(fader) == channel_name {
                self.apply_transient_fader_routing(fader, router);
            }
        }
        self.apply_transient_cough_routing(router);
    }

    fn apply_transient_fader_routing(
        &self,
        fader: FaderName,
        router: &mut EnumMap<BasicOutputDevice, bool>,
    ) {
        let (muted_to_x, muted_to_all, mute_function) = self.profile.get_mute_button_state(fader);
        self.apply_transient_channel_routing(muted_to_x, muted_to_all, mute_function, router);
    }

    fn apply_transient_cough_routing(&self, router: &mut EnumMap<BasicOutputDevice, bool>) {
        // Same deal, pull out the current state, make needed changes.
        let (_mute_toggle, muted_to_x, muted_to_all, mute_function) =
            self.profile.get_mute_chat_button_state();

        self.apply_transient_channel_routing(muted_to_x, muted_to_all, mute_function, router);
    }

    fn apply_transient_channel_routing(
        &self,
        muted_to_x: bool,
        muted_to_all: bool,
        mute_function: MuteFunction,
        router: &mut EnumMap<BasicOutputDevice, bool>,
    ) {
        if !muted_to_x || muted_to_all || mute_function == MuteFunction::All {
            return;
        }

        match mute_function {
            MuteFunction::All => {}
            MuteFunction::ToStream => router[BasicOutputDevice::BroadcastMix] = false,
            MuteFunction::ToVoiceChat => router[BasicOutputDevice::ChatMic] = false,
            MuteFunction::ToPhones => router[BasicOutputDevice::Headphones] = false,
            MuteFunction::ToLineOut => router[BasicOutputDevice::LineOut] = false,
        }
    }

    fn apply_routing(&mut self, input: BasicInputDevice) -> Result<()> {
        // Load the routing for this channel from the profile..
        let mut router = self.profile.get_router(input);
        self.apply_transient_routing(input, &mut router);
        debug!("Applying Routing to {:?}:", input);
        debug!("{:?}", router);

        self.apply_channel_routing(input, router)?;

        Ok(())
    }

    fn apply_mute_from_profile(&mut self, fader: FaderName) -> Result<()> {
        // Basically stripped down behaviour from handle_fader_mute which simply applies stuff.
        let channel = self.profile.get_fader_assignment(fader);

        let (muted_to_x, muted_to_all, mute_function) = self.profile.get_mute_button_state(fader);
        if muted_to_all || (muted_to_x && mute_function == MuteFunction::All) {
            // This channel should be fully muted
            self.goxlr.set_channel_state(channel, Muted)?;
        }

        // This channel isn't supposed to be muted (The Router will handle anything else).
        self.goxlr.set_channel_state(channel, Unmuted)?;
        Ok(())
    }

    fn apply_cough_from_profile(&mut self) -> Result<()> {
        // As above, but applies the cough profile.
        let (mute_toggle, muted_to_x, muted_to_all, mute_function) =
            self.profile.get_mute_chat_button_state();

        // Firstly, if toggle is to hold and anything is muted, clear it.
        if !mute_toggle && muted_to_x {
            self.profile.set_mute_chat_button_on(false);
            self.profile.set_mute_chat_button_blink(false);
            return Ok(());
        }

        if muted_to_all || (muted_to_x && mute_function == MuteFunction::All) {
            self.goxlr.set_channel_state(ChannelName::Mic, Muted)?;
        }
        Ok(())
    }

    async fn set_fader(&mut self, fader: FaderName, new_channel: ChannelName) -> Result<()> {
        // A couple of things need to happen when a fader change occurs depending on scenario..
        if new_channel == self.profile.get_fader_assignment(fader) {
            // We don't need to do anything at all in theory, set the fader anyway..
            if new_channel == ChannelName::Mic {
                self.profile.set_mic_fader_id(fader as u8);
            }

            self.goxlr.set_fader(fader, new_channel)?;
            return Ok(());
        }

        // Firstly, get the state and settings of the fader..
        let existing_channel = self.profile.get_fader_assignment(fader);

        // Go over the faders, see if the new channel is already bound..
        let mut fader_to_switch: Option<FaderName> = None;
        for fader_name in FaderName::iter() {
            if fader_name != fader && self.profile.get_fader_assignment(fader_name) == new_channel {
                fader_to_switch = Some(fader_name);
            }
        }

        if fader_to_switch.is_none() {
            // Whatever is on the fader already is going away, per windows behaviour we need to
            // ensure any mute behaviour is restored as it can no longer be tracked.
            let (muted_to_x, _muted_to_all, _mute_function) =
                self.profile.get_mute_button_state(fader);

            if muted_to_x {
                // Simulate a mute button tap, this should restore everything..
                self.handle_fader_mute(fader, false).await?;
            }

            // Check to see if we are dispatching of the mic channel, if so set the id.
            if existing_channel == ChannelName::Mic {
                self.profile.set_mic_fader_id(4);
            }

            // Now set the new fader..
            self.profile.set_fader_assignment(fader, new_channel);
            self.goxlr.set_fader(fader, new_channel)?;

            return Ok(());
        }

        // This will always be set here..
        let fader_to_switch = fader_to_switch.unwrap();

        // So we need to switch the faders and mute settings, but nothing else actually changes,
        // we'll simply switch the faders and mute buttons in the config, then apply to the
        // GoXLR.
        self.profile.switch_fader_assignment(fader, fader_to_switch);

        // Are either of the moves being done by the mic channel?
        if new_channel == ChannelName::Mic {
            self.profile.set_mic_fader_id(fader as u8);
        }

        if existing_channel == ChannelName::Mic {
            self.profile.set_mic_fader_id(fader_to_switch as u8);
        }

        // Now switch the faders on the GoXLR..
        self.goxlr.set_fader(fader, new_channel)?;
        self.goxlr.set_fader(fader_to_switch, existing_channel)?;

        // Finally update the button colours..
        self.update_button_states()?;

        Ok(())
    }

    fn get_fader_state(&self, fader: FaderName) -> FaderStatus {
        FaderStatus {
            channel: self.profile().get_fader_assignment(fader),
            mute_type: self.profile().get_mute_button_behaviour(fader),
        }
    }

    fn set_fader_display_from_profile(&mut self, fader: FaderName) -> Result<()> {
        self.goxlr.set_fader_display_mode(
            fader,
            self.profile.is_fader_gradient(fader),
            self.profile.is_fader_meter(fader),
        )?;
        Ok(())
    }

    fn get_bleep_volume(&self) -> i8 {
        // This should be fast, block on the request..
        let value = block_on(self.settings.get_device_bleep_volume(self.serial()));

        if let Some(bleep) = value {
            return bleep;
        }
        -14
    }

    fn load_colour_map(&mut self) -> Result<()> {
        // The new colour format occurred on different firmware versions depending on device,
        // so do the check here.

        let use_1_3_40_format: bool = match self.hardware.device_type {
            DeviceType::Unknown => true,
            DeviceType::Full => version_newer_or_equal_to(
                &self.hardware.versions.firmware,
                VersionNumber(1, 3, 40, 0),
            ),
            DeviceType::Mini => version_newer_or_equal_to(
                &self.hardware.versions.firmware,
                VersionNumber(1, 1, 8, 0),
            ),
        };

        let colour_map = self.profile.get_colour_map(use_1_3_40_format);

        if use_1_3_40_format {
            self.goxlr.set_button_colours_1_3_40(colour_map)?;
        } else {
            let mut map: [u8; 328] = [0; 328];
            map.copy_from_slice(&colour_map[0..328]);
            self.goxlr.set_button_colours(map)?;
        }

        Ok(())
    }

    fn apply_profile(&mut self) -> Result<()> {
        // Set volumes first, applying mute may modify stuff..
        debug!("Applying Profile..");

        debug!("Setting Faders..");
        // Prepare the faders, and configure channel mute states
        for fader in FaderName::iter() {
            debug!(
                "Setting Fader {} to {:?}",
                fader,
                self.profile.get_fader_assignment(fader)
            );
            self.goxlr
                .set_fader(fader, self.profile.get_fader_assignment(fader))?;

            debug!("Applying Mute Profile for {}", fader);
            self.apply_mute_from_profile(fader)?;
        }

        debug!("Applying Cough button settings..");
        self.apply_cough_from_profile()?;

        debug!("Loading Colour Map..");
        self.load_colour_map()?;

        debug!("Setting Fader display modes..");
        for fader in FaderName::iter() {
            debug!("Setting display for {}", fader);
            self.set_fader_display_from_profile(fader)?;
        }

        debug!("Setting Channel Volumes..");
        for channel in ChannelName::iter() {
            let channel_volume = self.profile.get_channel_volume(channel);
            debug!("Setting volume for {} to {}", channel, channel_volume);
            self.goxlr.set_volume(channel, channel_volume)?;
        }

        debug!("Updating button states..");
        self.update_button_states()?;

        debug!("Applying Routing..");
        // For profile load, we should configure all the input channels from the profile,
        // this is split so we can do tweaks in places where needed.
        for input in BasicInputDevice::iter() {
            self.apply_routing(input)?;
        }

        Ok(())
    }

    /// Applies a Set of Microphone Parameters based on input, designed this way
    /// so that commands and other abstract entities can apply a subset of params
    fn apply_mic_params(&mut self, params: HashSet<MicrophoneParamKey>) -> Result<()> {
        let mut vec = Vec::new();
        for param in params {
            vec.push((
                param,
                self.mic_profile
                    .get_param_value(param, self.serial(), self.settings),
            ));
        }
        self.goxlr.set_mic_param(vec.as_slice())?;
        Ok(())
    }

    fn apply_effects(&mut self, params: HashSet<EffectKey>) -> Result<()> {
        let mut vec = Vec::new();
        for effect in params {
            vec.push((
                effect,
                self.mic_profile.get_effect_value(
                    effect,
                    self.serial(),
                    self.settings,
                    self.profile(),
                ),
            ));
        }

        for effect in &vec {
            let (key, value) = effect;
            debug!("Setting {:?} to {}", key, value);
        }
        self.goxlr.set_effect_values(vec.as_slice())?;
        Ok(())
    }

    fn apply_mic_gain(&mut self) -> Result<()> {
        let mic_type = self.mic_profile.mic_type();
        let gain = self.mic_profile.mic_gains()[mic_type as usize];
        self.goxlr.set_microphone_gain(mic_type, gain)?;

        Ok(())
    }

    fn apply_mic_profile(&mut self) -> Result<()> {
        // Configure the microphone..
        self.apply_mic_gain()?;

        let mut keys = HashSet::new();
        for param in MicrophoneParamKey::iter() {
            keys.insert(param);
        }

        // Remove all gain settings, and re-add the relevant one.
        keys.remove(&MicrophoneParamKey::DynamicGain);
        keys.remove(&MicrophoneParamKey::CondenserGain);
        keys.remove(&MicrophoneParamKey::JackGain);
        keys.insert(self.mic_profile.mic_type().get_gain_param());

        self.apply_mic_params(keys)?;

        let mut keys = HashSet::new();
        keys.extend(self.mic_profile.get_common_keys());

        if self.hardware.device_type == DeviceType::Full {
            keys.extend(self.mic_profile.get_full_keys());
        }

        self.apply_effects(keys)?;

        if self.hardware.device_type == DeviceType::Full {
            self.load_effects()?;
            self.set_pitch_mode()?;
        }
        Ok(())
    }

    fn load_effects(&mut self) -> Result<()> {
        // For now, we'll simply set the knob positions, more to come!
        let mut value = self.profile.get_pitch_value();
        self.goxlr
            .set_encoder_value(EncoderName::Pitch, value as u8)?;

        value = self.profile.get_echo_value();
        self.goxlr
            .set_encoder_value(EncoderName::Echo, value as u8)?;

        value = self.profile.get_gender_value();
        self.goxlr
            .set_encoder_value(EncoderName::Gender, value as u8)?;

        value = self.profile.get_reverb_value();
        self.goxlr
            .set_encoder_value(EncoderName::Reverb, value as u8)?;

        Ok(())
    }

    fn set_pitch_mode(&mut self) -> Result<()> {
        if self.hardware.device_type != DeviceType::Full {
            // Not a Full GoXLR, nothing to do.
            return Ok(());
        }

        if self.profile.is_hardtune_pitch_enabled() {
            if self.profile.is_pitch_narrow() {
                self.goxlr.set_encoder_mode(EncoderName::Pitch, 3, 1)?;
            } else {
                self.goxlr.set_encoder_mode(EncoderName::Pitch, 3, 2)?;
            }
        } else {
            self.goxlr.set_encoder_mode(EncoderName::Pitch, 1, 4)?;
        }

        Ok(())
    }

    // Get the current time in millis..
    fn get_epoch_ms(&self) -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    }

    pub fn is_connected(&self) -> bool {
        self.goxlr.is_connected()
    }
}

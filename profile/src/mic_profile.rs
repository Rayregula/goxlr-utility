use crate::error::{ParseError, SaveError};
use crate::microphone::compressor::Compressor;
use crate::microphone::equalizer::Equalizer;
use crate::microphone::equalizer_mini::EqualizerMini;
use crate::microphone::gate::Gate;
use crate::microphone::mic_setup::MicSetup;
use crate::microphone::ui_setup::UiSetup;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::raw::c_float;
use std::path::Path;
use xml::reader::XmlEvent as XmlReaderEvent;
use xml::writer::events::StartElementBuilder;
use xml::writer::XmlEvent as XmlWriterEvent;
use xml::{EmitterConfig, EventReader};

#[derive(Debug)]
pub struct MicProfileSettings {
    equalizer: Equalizer,
    equalizer_mini: EqualizerMini,
    compressor: Compressor,
    gate: Gate,
    deess: u8,
    mic_setup: MicSetup,
    ui_setup: UiSetup,
}

impl MicProfileSettings {
    pub fn load<R: Read>(read: R) -> Result<Self, ParseError> {
        let parser = EventReader::new(read);

        let mut equalizer = Equalizer::new();
        let mut equalizer_mini = EqualizerMini::new();
        let mut compressor = Compressor::new();
        let mut gate = Gate::new();
        let mut deess = 0;
        let mut mic_setup = MicSetup::new();
        let mut ui_setup = UiSetup::new();

        for e in parser {
            match e {
                Ok(XmlReaderEvent::StartElement {
                    name, attributes, ..
                }) => {
                    if name.local_name == "dspTreeMicProfile" {
                        // Ok, this is an incredibly large tag, with many settings (30 or so), so
                        // we split it into 3 separate elements.
                        equalizer.parse_equaliser(&attributes)?;
                        equalizer_mini.parse_equaliser(&attributes)?;
                        compressor.parse_compressor(&attributes)?;
                        gate.parse_gate(&attributes)?;

                        // Before we're done here, there's a single attribute that doesn't fit into
                        // any of the above categories, find it and handle it here..
                        for attr in &attributes {
                            if attr.name.local_name == "MIC_DEESS_AMOUNT" {
                                deess = attr.value.parse::<c_float>()? as u8;
                                break;
                            }
                        }

                        continue;
                    }

                    if name.local_name == "setupTreeMicProfile" {
                        mic_setup.parse_config(&attributes)?;
                        continue;
                    }

                    if name.local_name == "micProfileUIMicProfile" {
                        ui_setup.parse_ui(&attributes)?;
                        continue;
                    }

                    if name.local_name == "MicProfileTree" {
                        continue;
                    }

                    println!("Unhandled Tag: {}", name.local_name);
                }
                Err(e) => {
                    println!("Error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok(Self {
            equalizer,
            equalizer_mini,
            compressor,
            gate,
            deess,
            mic_setup,
            ui_setup,
        })
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), SaveError> {
        dbg!("Saving File: {}", &path.as_ref());

        let out_file = File::create(path)?;
        self.write_to(out_file)?;

        Ok(())
    }

    pub fn write_to<W: Write>(&self, mut sink: W) -> Result<(), xml::writer::Error> {
        // Create the file, and the writer..
        let mut writer = EmitterConfig::new()
            .perform_indent(true)
            .create_writer(&mut sink);

        writer.write(XmlWriterEvent::start_element("MicProfileTree"))?;

        // First, we need to write the EQ, Compressor and Gate..
        let mut attributes: HashMap<String, String> = HashMap::default();

        // The mini and main can both have configs in the same file.
        self.equalizer.write_equaliser(&mut attributes);
        self.equalizer_mini.write_equaliser(&mut attributes);
        self.compressor.write_compressor(&mut attributes);
        self.gate.write_gate(&mut attributes);
        attributes.insert("MIC_DEESS_AMOUNT".to_string(), format!("{}", self.deess));

        let mut element: StartElementBuilder = XmlWriterEvent::start_element("dspTreeMicProfile");
        for (key, value) in &attributes {
            element = element.attr(key.as_str(), value.as_str());
        }
        writer.write(element)?;
        writer.write(XmlWriterEvent::end_element())?;

        self.mic_setup.write_config(&mut writer)?;
        self.ui_setup.write_ui(&mut writer)?;

        writer.write(XmlWriterEvent::end_element())?;

        Ok(())
    }

    pub fn setup_mut(&mut self) -> &mut MicSetup {
        &mut self.mic_setup
    }
    pub fn setup(&self) -> &MicSetup {
        &self.mic_setup
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }
    pub fn gate_mut(&mut self) -> &mut Gate {
        &mut self.gate
    }
    pub fn compressor(&self) -> &Compressor {
        &self.compressor
    }
    pub fn compressor_mut(&mut self) -> &mut Compressor {
        &mut self.compressor
    }
    pub fn equalizer(&self) -> &Equalizer {
        &self.equalizer
    }
    pub fn equalizer_mut(&mut self) -> &mut Equalizer {
        &mut self.equalizer
    }

    pub fn equalizer_mini(&self) -> &EqualizerMini {
        &self.equalizer_mini
    }
    pub fn equalizer_mini_mut(&mut self) -> &mut EqualizerMini {
        &mut self.equalizer_mini
    }
    pub fn deess(&self) -> u8 {
        self.deess
    }
}

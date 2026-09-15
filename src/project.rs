use anyhow::{Context, Result, bail, ensure};
use serialport::{SerialPort, SerialPortType, available_ports};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const BAUD_RATE: u32 = 115_200;
const BLOCK_SIZE: usize = 96;
const MAX_PROJECT_INDEX: u8 = 15;

// LinnStrument projects are only a few KB. This is a generous sanity
// ceiling so a corrupt device reply can't trigger a huge allocation
// (save_project reads the size from the device, unlike restore which
// validates against its input file).
const MAX_PROJECT_SIZE: usize = 8 * 1024 * 1024;

// Maximum retransmissions of a single block before giving up, so a
// persistently failing link fails loudly instead of spinning forever.
const MAX_BLOCK_RETRIES: usize = 5;

const HANDSHAKE: &[u8] = b"5, 4, 3, 2, 1 ...\n";
const HANDSHAKE_RESPONSE: &[u8] = b"LinnStruments are go!\n";
const ACK: &[u8] = b"ACK\n";

const OPEN_SETTLE_TIME: Duration = Duration::from_secs(1);
const BLOCK_SETTLE_TIME: Duration = Duration::from_millis(20);
const PORT_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct LinnStrument {
    port: Box<dyn SerialPort>,
}

impl LinnStrument {
    pub fn open(initial_port: &str) -> Result<Self> {
        let mut port_name = initial_port.to_owned();

        for attempt in 1..=5 {
            eprintln!(
                "Opening LinnStrument on {port_name} \
                 (attempt {attempt}/5)"
            );

            match Self::try_open_and_handshake(&port_name) {
                Ok(instrument) => return Ok(instrument),

                Err(error) => {
                    eprintln!("Connection attempt failed:\n{error:#}");

                    if attempt == 5 {
                        break;
                    }

                    eprintln!("Waiting for LinnStrument to reappear...");

                    port_name = wait_for_reenumerated_port(&port_name, PORT_SEARCH_TIMEOUT)?;
                }
            }
        }

        bail!("could not establish communication with LinnStrument")
    }

    fn try_open_and_handshake(port_name: &str) -> Result<Self> {
        let port = serialport::new(port_name, BAUD_RATE)
            .timeout(Duration::from_secs(3))
            .open()
            .with_context(|| format!("could not open serial port {port_name}"))?;

        let mut instrument = Self { port };

        // Opening the USB serial device may reset the instrument.
        std::thread::sleep(OPEN_SETTLE_TIME);

        instrument.handshake_once()?;

        Ok(instrument)
    }

    fn handshake_once(&mut self) -> Result<()> {
        self.port
            .write_all(HANDSHAKE)
            .context("handshake write failed")?;

        self.port.flush().context("handshake flush failed")?;

        let response = self
            .read_line()
            .context("could not read handshake response")?;

        ensure!(
            response == HANDSHAKE_RESPONSE,
            "unexpected handshake response: {:?}",
            String::from_utf8_lossy(&response)
        );

        Ok(())
    }

    pub fn save_project(&mut self, project_index: u8, output_path: impl AsRef<Path>) -> Result<()> {
        validate_project_index(project_index)?;

        self.write_command(b'j')?;
        self.expect_ack("single-project command")?;

        self.port
            .write_all(&[project_index])
            .context("could not write project index")?;
        self.port.flush()?;

        self.expect_ack("project index")?;

        let version = self.read_u8().context("could not read project version")?;
        let project_size = self.read_i32_le().context("could not read project size")?;

        ensure!(
            project_size >= 0,
            "device returned negative project size: {project_size}"
        );

        let project_size = project_size as usize;

        ensure!(
            project_size <= MAX_PROJECT_SIZE,
            "device returned implausibly large project size: \
             {project_size} bytes (limit {MAX_PROJECT_SIZE})"
        );

        println!(
            "Reading project {}: version {}, {} bytes",
            project_index + 1,
            version,
            project_size
        );

        let mut project_data = Vec::with_capacity(project_size);

        while project_data.len() < project_size {
            // The original updater gives the instrument time to prepare
            // the next block.
            std::thread::sleep(BLOCK_SETTLE_TIME);

            let offset = project_data.len();
            let remaining = project_size - offset;
            let block_size = remaining.min(BLOCK_SIZE);

            let mut block = vec![0u8; block_size];

            self.read_block(&mut block, offset)?;

            loop {
                match self.negotiate_incoming_crc(&block)? {
                    CrcResult::Accepted => break,

                    CrcResult::Retry => {
                        eprintln!(
                            "CRC rejected at offset {offset}; \
                             waiting for retransmitted block"
                        );

                        self.read_block(&mut block, offset)?;
                    }
                }
            }

            project_data.extend_from_slice(&block);

            println!("Read {}/{} bytes", project_data.len(), project_size);
        }

        self.expect_ack("project transfer completion")?;

        let total_crc = crc32(&project_data);

        let mut file = Vec::with_capacity(1 + 4 + project_data.len() + 4);

        file.push(version);
        file.extend_from_slice(&(project_size as i32).to_le_bytes());
        file.extend_from_slice(&project_data);
        file.extend_from_slice(&total_crc.to_le_bytes());

        atomic_write(output_path.as_ref(), &file)?;

        println!(
            "Saved project {}: {} bytes, CRC {total_crc:08x}",
            project_index + 1,
            project_size
        );

        Ok(())
    }

    pub fn load_project(&mut self, project_index: u8, input_path: impl AsRef<Path>) -> Result<()> {
        validate_project_index(project_index)?;

        let file = fs::read(input_path.as_ref())
            .with_context(|| format!("could not read {}", input_path.as_ref().display()))?;

        let project = ProjectFile::parse(&file)?;

        println!(
            "Loading project {}: version {}, {} bytes",
            project_index + 1,
            project.version,
            project.data.len()
        );

        self.write_command(b'q')?;
        self.expect_ack("restore-project command")?;

        self.port
            .write_all(&[project.version])
            .context("could not write project version")?;
        self.port.flush()?;
        self.expect_ack("project version")?;

        let size = project.data.len();

        ensure!(
            size <= i32::MAX as usize,
            "project is too large: {size} bytes"
        );

        self.port
            .write_all(&(size as i32).to_le_bytes())
            .context("could not write project size")?;
        self.port.flush()?;
        self.expect_ack("project size")?;

        self.port
            .write_all(&[project_index])
            .context("could not write project index")?;
        self.port.flush()?;
        self.expect_ack("project index")?;

        self.write_block_stream(project.data)?;

        self.expect_ack("project transfer completion")?;

        println!(
            "Restored project {}: {} bytes",
            project_index + 1,
            project.data.len()
        );

        Ok(())
    }

    pub fn save_settings(&mut self, output_path: impl AsRef<Path>) -> Result<()> {
        self.write_command(b's')?;
        self.expect_ack("read-settings command")?;

        // The size reported by the device includes the version byte.
        let settings_size = self.read_i32_le().context("could not read settings size")?;

        ensure!(
            settings_size >= 1,
            "device returned invalid settings size: {settings_size}"
        );

        let settings_size = settings_size as usize;

        ensure!(
            settings_size <= MAX_PROJECT_SIZE,
            "device returned implausibly large settings size: \
             {settings_size} bytes (limit {MAX_PROJECT_SIZE})"
        );

        println!("Reading settings: {settings_size} bytes");

        let mut settings_data = Vec::with_capacity(settings_size);
        let mut version: Option<u8> = None;

        while settings_data.len() < settings_size {
            // The original updater gives the instrument time to prepare
            // the next block.
            std::thread::sleep(BLOCK_SETTLE_TIME);

            let offset = settings_data.len();
            let remaining = settings_size - offset;
            let block_size = remaining.min(BLOCK_SIZE);

            let mut block = vec![0u8; block_size];

            self.read_block(&mut block, offset)?;

            // The version is the first byte of the transfer; it isn't
            // known until the first block has arrived.
            if version.is_none() {
                version = Some(block[0]);
            }

            match version.unwrap() {
                // 2.0.0-beta1/beta2 report settings blocks without CRC.
                9 => {
                    self.port.write_all(b"a")?;
                    self.port.flush()?;
                }

                // 2.0.0-beta3 and later negotiate a CRC per block.
                v if v >= 10 => loop {
                    match self.negotiate_incoming_crc(&block)? {
                        CrcResult::Accepted => break,

                        CrcResult::Retry => {
                            eprintln!(
                                "CRC rejected at offset {offset}; \
                                     waiting for retransmitted block"
                            );

                            self.read_block(&mut block, offset)?;
                        }
                    }
                },

                other => bail!("unsupported settings version: {other}"),
            }

            settings_data.extend_from_slice(&block);

            println!("Read {}/{} bytes", settings_data.len(), settings_size);
        }

        self.expect_ack("settings transfer completion")?;

        let version = version.context("could not determine settings version")?;

        // settings_data[0] is the version byte; the rest is the payload.
        let payload = &settings_data[1..];
        let total_crc = crc32(payload);

        let mut file = Vec::with_capacity(1 + 4 + payload.len() + 4);

        file.push(version);
        file.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        file.extend_from_slice(payload);
        file.extend_from_slice(&total_crc.to_le_bytes());

        atomic_write(output_path.as_ref(), &file)?;

        println!(
            "Saved settings: version {version}, {} bytes of data, \
             CRC {total_crc:08x}",
            payload.len()
        );

        Ok(())
    }

    pub fn load_settings(&mut self, input_path: impl AsRef<Path>) -> Result<()> {
        let file = fs::read(input_path.as_ref())
            .with_context(|| format!("could not read {}", input_path.as_ref().display()))?;

        let settings = SettingsFile::parse(&file)?;

        println!(
            "Loading settings: version {}, {} bytes",
            settings.version,
            settings.data.len()
        );

        self.write_command(b'r')?;
        self.expect_ack("restore-settings command")?;

        // The size sent to the device includes the version byte.
        let total_size = settings.data.len() + 1;

        ensure!(
            total_size <= i32::MAX as usize,
            "settings are too large: {total_size} bytes"
        );

        self.port
            .write_all(&(total_size as i32).to_le_bytes())
            .context("could not write settings size")?;
        self.port.flush()?;
        self.expect_ack("settings size")?;

        // The restore protocol has no separate version step, so the
        // version byte is streamed as the first byte of the data.
        let mut blob = Vec::with_capacity(total_size);
        blob.push(settings.version);
        blob.extend_from_slice(settings.data);

        self.write_block_stream(&blob)?;

        self.expect_ack("settings transfer completion")?;

        println!(
            "Restored settings: version {}, {} bytes",
            settings.version,
            settings.data.len()
        );

        Ok(())
    }

    /// Stream `data` to the device in 96-byte blocks, negotiating a CRC
    /// per block and allowing bounded retransmission on rejection.
    fn write_block_stream(&mut self, data: &[u8]) -> Result<()> {
        let mut offset = 0;

        while offset < data.len() {
            let end = (offset + BLOCK_SIZE).min(data.len());
            let block = &data[offset..end];

            let mut retries = 0;

            loop {
                self.port
                    .write_all(block)
                    .with_context(|| format!("could not write block at offset {offset}"))?;

                self.port.flush()?;

                match self.negotiate_outgoing_crc(block)? {
                    CrcResult::Accepted => break,

                    CrcResult::Retry => {
                        retries += 1;

                        ensure!(
                            retries <= MAX_BLOCK_RETRIES,
                            "device rejected block at offset {offset} \
                             {MAX_BLOCK_RETRIES} times; giving up"
                        );

                        eprintln!(
                            "Device rejected block at offset {offset}; \
                             retrying ({retries}/{MAX_BLOCK_RETRIES})"
                        );
                    }
                }
            }

            offset = end;

            println!("Wrote {offset}/{} bytes", data.len());
        }

        Ok(())
    }

    fn read_block(&mut self, block: &mut [u8], offset: usize) -> Result<()> {
        let mut received = 0;

        while received < block.len() {
            match self.port.read(&mut block[received..]) {
                Ok(0) => {
                    bail!(
                        "serial device returned EOF at offset {offset}; \
                         received {received}/{} bytes",
                        block.len()
                    );
                }

                Ok(count) => {
                    received += count;
                }

                Err(error) if error.kind() == ErrorKind::TimedOut => {
                    bail!(
                        "timeout at offset {offset}; \
                         received {received}/{} bytes",
                        block.len()
                    );
                }

                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "could not read project block at offset {offset}; \
                             received {received}/{} bytes",
                            block.len()
                        )
                    });
                }
            }
        }

        Ok(())
    }

    fn write_command(&mut self, command: u8) -> Result<()> {
        self.port.write_all(&[command])?;
        self.port.flush()?;
        Ok(())
    }

    fn expect_ack(&mut self, operation: &str) -> Result<()> {
        let response = self.read_line()?;

        ensure!(
            response == ACK,
            "expected ACK after {operation}, received {:?}",
            String::from_utf8_lossy(&response)
        );

        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8> {
        let mut byte = [0u8; 1];
        self.port.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    fn read_i32_le(&mut self) -> Result<i32> {
        let mut bytes = [0u8; 4];
        self.port.read_exact(&mut bytes)?;
        Ok(i32::from_le_bytes(bytes))
    }

    fn read_u32_le(&mut self) -> Result<u32> {
        let mut bytes = [0u8; 4];
        self.port.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();

        loop {
            let byte = self.read_u8()?;
            line.push(byte);

            if byte == b'\n' {
                return Ok(line);
            }

            ensure!(
                line.len() <= 256,
                "serial line exceeded 256 bytes without newline"
            );
        }
    }

    fn negotiate_incoming_crc(&mut self, block: &[u8]) -> Result<CrcResult> {
        self.port.write_all(b"c")?;
        self.port.flush()?;

        let remote_crc = self.read_u32_le()?;
        let local_crc = crc32(block);

        if remote_crc != local_crc {
            self.port.write_all(b"w")?;
            self.port.flush()?;

            eprintln!(
                "block CRC mismatch: local {local_crc:08x}, \
                 remote {remote_crc:08x}"
            );

            return Ok(CrcResult::Retry);
        }

        self.port.write_all(b"o")?;
        self.port.flush()?;

        Ok(CrcResult::Accepted)
    }

    fn negotiate_outgoing_crc(&mut self, block: &[u8]) -> Result<CrcResult> {
        let command = self.read_u8()?;

        ensure!(
            command == b'c',
            "expected device CRC request, received 0x{command:02x}"
        );

        let crc = crc32(block);

        self.port.write_all(&crc.to_le_bytes())?;
        self.port.flush()?;

        match self.read_u8()? {
            b'o' => Ok(CrcResult::Accepted),
            b'w' => Ok(CrcResult::Retry),

            other => bail!("expected CRC result 'o' or 'w', received 0x{other:02x}"),
        }
    }
}

fn wait_for_reenumerated_port(previous_port: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut previous_was_absent = false;

    loop {
        let ports = available_ports().unwrap_or_default();

        let previous_present = ports.iter().any(|port| port.port_name == previous_port);

        if !previous_present {
            previous_was_absent = true;
        }

        if previous_was_absent
            && let Some(port) = ports
                .iter()
                .find(|port| is_likely_linnstrument_port(&port.port_type))
        {
            return Ok(port.port_name.clone());
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for LinnStrument to reappear \
                 after {previous_port}"
            );
        }

        std::thread::sleep(Duration::from_millis(250));
    }
}

/// USB vendor/product IDs reported by the LinnStrument's device/interface.
const LINN_VENDOR_ID: u16 = 0xF055;
const LINN_PRODUCT_ID: u16 = 0x0070;

/// Locate the serial port of the connected LinnStrument.
pub fn find_linnstrument_port() -> Result<String> {
    let ports = available_ports().context("could not enumerate serial ports")?;

    for port in &ports {
        if is_likely_linnstrument_port(&port.port_type) {
            return Ok(port.port_name.clone());
        }
    }

    bail!(
        "could not find a LinnStrument serial device; \
         connect it and run `list` to see available ports"
    )
}

/// Does this USB vendor/product ID pair identify a LinnStrument?
pub fn is_linnstrument_vid_pid(vid: u16, pid: u16) -> bool {
    vid == LINN_VENDOR_ID && pid == LINN_PRODUCT_ID
}

/// Heuristic: does a device with these USB manufacturer/product strings
/// look like a LinnStrument? Shared between `list` (main.rs) and the
/// re-enumeration search here.
pub fn is_likely_linnstrument(manufacturer: Option<&str>, product: Option<&str>) -> bool {
    let text = format!(
        "{} {}",
        manufacturer.unwrap_or_default(),
        product.unwrap_or_default()
    )
    .to_ascii_lowercase();

    text.contains("linnstrument")
        || text.contains("roger linn")
        || text.contains("arduino due")
        || text.contains("sam3x")
}

fn is_likely_linnstrument_port(port_type: &SerialPortType) -> bool {
    match port_type {
        SerialPortType::UsbPort(info) => {
            is_linnstrument_vid_pid(info.vid, info.pid)
                || is_likely_linnstrument(info.manufacturer.as_deref(), info.product.as_deref())
        }

        _ => false,
    }
}

enum CrcResult {
    Accepted,
    Retry,
}

struct ProjectFile<'a> {
    version: u8,
    data: &'a [u8],
}

impl<'a> ProjectFile<'a> {
    fn parse(file: &'a [u8]) -> Result<Self> {
        ensure!(
            file.len() >= 1 + 4 + 4,
            "project file is too small: {} bytes",
            file.len()
        );

        let version = file[0];

        ensure!(version >= 10, "unsupported project version: {version}");

        let declared_size = i32::from_le_bytes(file[1..5].try_into().unwrap());

        ensure!(
            declared_size >= 0,
            "project file contains negative project size"
        );

        let declared_size = declared_size as usize;
        let expected_file_size = 1 + 4 + declared_size + 4;

        ensure!(
            file.len() == expected_file_size,
            "invalid project file size: declared data size is {declared_size}, \
             but file contains {} bytes",
            file.len()
        );

        let data_start = 5;
        let data_end = data_start + declared_size;
        let data = &file[data_start..data_end];

        let stored_crc = u32::from_le_bytes(file[data_end..data_end + 4].try_into().unwrap());

        let actual_crc = crc32(data);

        ensure!(
            stored_crc == actual_crc,
            "project CRC mismatch: stored {stored_crc:08x}, \
             calculated {actual_crc:08x}"
        );

        Ok(Self { version, data })
    }
}

struct SettingsFile<'a> {
    version: u8,
    data: &'a [u8],
}

impl<'a> SettingsFile<'a> {
    fn parse(file: &'a [u8]) -> Result<Self> {
        ensure!(
            file.len() >= 1 + 4 + 4,
            "settings file is too small: {} bytes",
            file.len()
        );

        let version = file[0];

        ensure!(version >= 10, "unsupported settings version: {version}");

        let declared_size = i32::from_le_bytes(file[1..5].try_into().unwrap());

        ensure!(
            declared_size >= 0,
            "settings file contains negative data size"
        );

        let declared_size = declared_size as usize;
        let expected_file_size = 1 + 4 + declared_size + 4;

        ensure!(
            file.len() == expected_file_size,
            "invalid settings file size: declared data size is {declared_size}, \
             but file contains {} bytes",
            file.len()
        );

        let data_start = 5;
        let data_end = data_start + declared_size;
        let data = &file[data_start..data_end];

        let stored_crc = u32::from_le_bytes(file[data_end..data_end + 4].try_into().unwrap());

        let actual_crc = crc32(data);

        ensure!(
            stored_crc == actual_crc,
            "settings CRC mismatch: stored {stored_crc:08x}, \
             calculated {actual_crc:08x}"
        );

        Ok(Self { version, data })
    }
}

fn validate_project_index(index: u8) -> Result<()> {
    ensure!(
        index <= MAX_PROJECT_INDEX,
        "project index must be between 0 and {MAX_PROJECT_INDEX}"
    );

    Ok(())
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");

    fs::write(&temporary, data)
        .with_context(|| format!("could not write {}", temporary.display()))?;

    fs::rename(&temporary, path)
        .with_context(|| format!("could not rename {}", temporary.display()))?;

    Ok(())
}

const CRC_TABLE: [u32; 16] = [
    0x0000_0000,
    0x1db7_1064,
    0x3b6e_20c8,
    0x26d9_30ac,
    0x76dc_4190,
    0x6b6b_51f4,
    0x4db2_6158,
    0x5005_713c,
    0xedb8_8320,
    0xf00f_9344,
    0xd6d6_a3e8,
    0xcb61_b38c,
    0x9b64_c2b0,
    0x86d3_d2d4,
    0xa00a_e278,
    0xbdbd_f21c,
];

fn crc_update(mut crc: u32, data: u8) -> u32 {
    let mut table_index = ((crc ^ data as u32) as usize) & 0x0f;
    crc = CRC_TABLE[table_index] ^ (crc >> 4);

    table_index = ((crc ^ ((data as u32) >> 4)) as usize) & 0x0f;
    CRC_TABLE[table_index] ^ (crc >> 4)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;

    for &byte in data {
        crc = crc_update(crc, byte);
    }

    !crc
}

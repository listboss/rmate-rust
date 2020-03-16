use base64;
use log::*;
use socket2::{Domain, Type};
use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{canonicalize, metadata};
use std::io::prelude::*;
use std::io::{BufRead, BufReader, BufWriter, Error, ErrorKind, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

// use std::path::PathBuf;

// TODO: make a backup copy of files being saved? <08-03-20, yourname> //
// TODO: create struct to store opsions for each file opened <08-03-20, yourname> //
// TODO: use clap for argument parsing <08-03-20, yourname> //
// TODO: read config files (/etc/rmate.conf)? <08-03-20, yourname> //
// TODO: warn user about openning read-only files <08-03-20, yourname> //
// TODO: use 'envy' crate to parse RMATE_* env. variables. <15-03-20, yourname> //
// TODO: use 'group' feature of clap/structopt to parse: -m name1 namefile1 file1 file2 -m name2 namefile2 file3 <15-03-20, hamid> //

mod settings;
use settings::OpenedBuffer;
use settings::Settings;
use structopt::StructOpt;

fn main() -> Result<(), String> {
    let settings = Settings::from_args();

    // println!("verbose: {}", settings.verbose);
    let level;
    match std::env::var("RUST_LOG") {
        Err(_) => {
            match settings.verbose {
                0 => level = "",
                1 => level = "info",
                2 => level = "debug",
                _ => level = "trace",
            }
            std::env::set_var("RUST_LOG", level);
        }
        _ => {}
    }
    env_logger::init();
    // info!("verbose: {}", settings.verbose);
    // debug!("verbose: {}", settings.verbose);
    // trace!("verbose: {}", settings.verbose);
    // warn!("verbose: {}", settings.verbose);
    // error!("verbose: {}", settings.verbose);

    let socket = connect_to_editor(&settings).map_err(|e| e.to_string())?;
    let buffers = get_opened_buffers(&settings)?;
    let buffers = open_file_in_remote(&socket, buffers)?;
    handle_remote(socket, buffers).map_err(|e| e.to_string())?;
    Ok(())
}

fn connect_to_editor(settings: &Settings) -> Result<socket2::Socket, std::io::Error> {
    let socket = socket2::Socket::new(Domain::ipv4(), Type::stream(), None).unwrap();

    debug!("Host: {}", settings.host);
    let addr_srv = if settings.host == "localhost" {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    } else {
        settings
            .host
            .parse::<IpAddr>()
            .map_err(|e| Error::new(ErrorKind::AddrNotAvailable, e.to_string()))?
    };
    let port = settings.port;
    let addr_srv = std::net::SocketAddr::new(addr_srv, port).into();

    debug!("About to connect to {:?}", addr_srv);
    socket.connect(&addr_srv)?;
    trace!(
        "Socket details: \n\tmy address: {:?}\n\tremote address {:?}",
        socket.local_addr()?,
        socket.peer_addr()?
    );
    Ok(socket)
}

fn get_opened_buffers(settings: &Settings) -> Result<HashMap<String, OpenedBuffer>, String> {
    let mut buffers = HashMap::new();
    for (idx, file) in settings.files.iter().enumerate() {
        let filename_canon = canonicalize(file).map_err(|e| e.to_string())?;
        let file_name_string;
        if settings.names.len() > idx {
            file_name_string = settings.names[idx].clone();
        } else {
            file_name_string = filename_canon
                .file_name()
                .ok_or("no valid file name found in input argument".to_string())?
                .to_os_string();
        }
        let md = metadata(&filename_canon).map_err(|e| e.to_string())?;
        if md.is_dir() {
            return Err("openning directory not supported".to_string());
        }
        let canwrite = is_writable(&filename_canon, &md);
        if !canwrite {
            warn!("{:?} is readonly!", filename_canon);
        }
        if !(canwrite || settings.force) {
            return Err(format!(
                "File {} is read-only, use -f/--force to open it anyway",
                file_name_string.to_string_lossy()
            ));
        }
        let filesize = md.len();
        let rand_temp_file = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut encoded_fn = String::with_capacity(512);
        base64::encode_config_buf(
            filename_canon.to_string_lossy().as_bytes(),
            base64::STANDARD,
            &mut encoded_fn,
        );
        buffers.insert(
            file_name_string.to_string_lossy().into_owned(),
            OpenedBuffer {
                canon_path: filename_canon,
                display_name: file_name_string.clone(),
                canwrite: canwrite,
                temp_file: rand_temp_file,
                size: filesize,
            },
        );
    }
    trace!("All opened buffers:\n{:#?}", &buffers);
    Ok(buffers)
}
fn open_file_in_remote(
    socket: &socket2::Socket,
    buffers: HashMap<String, OpenedBuffer>,
) -> Result<HashMap<String, OpenedBuffer>, String> {
    let bsize = socket.recv_buffer_size().map_err(|e| e.to_string())?;
    debug!("Socket recv buffer: {}", bsize);
    let bsize = socket.send_buffer_size().map_err(|e| e.to_string())?;
    debug!("Socket send buffer: {}", bsize);
    {
        let mut buf_writer = BufWriter::with_capacity(bsize, socket);
        for (token, opened_buffer) in buffers.iter() {
            let mut total = 0usize;
            buf_writer
                .write_fmt(format_args!(
                    concat!(
                        "open\ndisplay-name: {}\n",
                        "real-path: {}\ndata-on-save: yes\nre-activate: yes\n",
                        "token: {}\ndata: {}\n"
                    ),
                    opened_buffer.display_name.to_string_lossy(),
                    opened_buffer.canon_path.to_string_lossy(),
                    token,
                    opened_buffer.size,
                ))
                .map_err(|e| e.to_string())?;
            let fp = File::open(&opened_buffer.canon_path).map_err(|e| e.to_string())?;
            let mut buf_reader = BufReader::with_capacity(bsize, fp);
            loop {
                let buffer = buf_reader.fill_buf().map_err(|e| e.to_string())?;
                let length = buffer.len();
                if length == 0 {
                    debug!(
                        "read & sent all of input file: {}",
                        opened_buffer.canon_path.to_string_lossy()
                    );
                    break;
                }
                total += length;
                buf_writer.write_all(&buffer).map_err(|e| e.to_string())?;
                trace!("  sent {} / {}", length, total);
                buf_reader.consume(length);
            }
            let _n = buf_writer
                .write_fmt(format_args!("\n.\n"))
                .map_err(|e| e.to_string());
            debug!(
                "  read {} (out of {} bytes) from input file.",
                total, opened_buffer.size
            );
            info!("Opened {:?}", opened_buffer.canon_path);
        }
    }

    let mut b = [0u8; 512];
    debug!("Waiting for remote editor to identiy itself...");
    let n = socket.recv(&mut b).map_err(|e| e.to_string())?;
    assert!(n < 512);
    debug!(
        "Connected to remote app: {}",
        String::from_utf8_lossy(&b[0..n]).trim()
    );
    Ok(buffers)
}

fn handle_remote(
    socket: socket2::Socket,
    mut opened_buffers: HashMap<String, OpenedBuffer>,
) -> Result<(), std::io::Error> {
    let mut total = 0;
    debug!("Waiting for editor's instructions...");
    let mut myline = String::with_capacity(128);
    let bsize = socket.recv_buffer_size()?;
    trace!("socket recv size: {}", bsize);
    let mut buffer_reader = BufReader::with_capacity(bsize, &socket);

    // Wait for commands from remote app
    while buffer_reader.read_line(&mut myline)? != 0 {
        debug!("Received line from editor (trimmed): >>{}<<", myline.trim());
        match myline.trim() {
            // close the buffer for a file
            "close" => {
                trace!("--> About to close_buffer()");
                myline.clear();
                close_buffer(&mut opened_buffers, &mut buffer_reader);
            }
            // save the buffer to a file
            "save" => {
                trace!("--> About to call write_to_disk()");
                myline.clear();
                match write_to_disk(&mut opened_buffers, &mut buffer_reader) {
                    Ok(n) => total += n,
                    Err(e) => error!("Couldn't save: {}", e.to_string()),
                }
            }
            _ => {
                if myline.trim() == "" {
                    trace!("--> Recvd empty line from editor");
                    continue;
                } else {
                    return Err(Error::new(ErrorKind::Other, "unrecognized shit"));
                }
            }
        }
    }
    trace!("Cumulative total bytes saved: {}", total);
    Ok(())
}

fn close_buffer(
    opened_buffers: &mut HashMap<String, OpenedBuffer>,
    buffer_reader: &mut BufReader<&socket2::Socket>,
) {
    let mut myline = String::with_capacity(128);

    while let Ok(n) = buffer_reader.read_line(&mut myline) {
        if n == 0 || myline.trim() == "" {
            trace!("Finished receiving closing instructions");
            break;
        }
        let command: Vec<&str> = myline.trim().splitn(2, ":").collect::<Vec<&str>>();
        trace!("  close instruction:\t{:?}", command);
        let (_, closed_buffer) = opened_buffers.remove_entry(command[1].trim()).unwrap();
        info!("Closed: {:?}", closed_buffer.canon_path.as_os_str());
        myline.clear();
    }
}

fn write_to_disk(
    opened_buffers: &mut HashMap<String, OpenedBuffer>,
    buffer_reader: &mut BufReader<&socket2::Socket>,
) -> Result<usize, std::io::Error> {
    let mut myline = String::with_capacity(128);
    buffer_reader.read_line(&mut myline)?;
    trace!("  save instruction:\t{:?}", myline.trim());
    let token = myline.trim().rsplitn(2, ":").collect::<Vec<&str>>()[0]
        .trim()
        .to_string();
    myline.clear();
    trace!("  token: >{}<", token);

    buffer_reader.read_line(&mut myline)?;
    trace!("  save instruction:\t{:?}", myline.trim());
    let data_size = myline.rsplitn(2, ":").collect::<Vec<&str>>()[0]
        .trim()
        .parse::<usize>()
        .unwrap();
    trace!("  save size:\t{:?}", data_size);
    myline.clear();
    trace!("  token: {:?}", token);
    trace!(
        "  display-name: {:?}",
        opened_buffers.get(&token).unwrap().display_name
    );
    let mut total = 0usize;
    {
        let rand_temp_file = &mut opened_buffers.get_mut(&token).unwrap().temp_file;
        rand_temp_file.seek(SeekFrom::Start(0))?;
        let mut buf_writer = BufWriter::with_capacity(1024, rand_temp_file);
        loop {
            let buffer = buffer_reader.fill_buf()?;
            let length = buffer.len();
            total += length;
            if total >= data_size {
                let corrected_last_length = length - (total - data_size);
                trace!("Total recvd: {}", total);
                trace!("Actual file size: {}", data_size);
                trace!("  difference: {}", total - data_size);
                buf_writer.write_all(&buffer[..corrected_last_length])?;
                buffer_reader.consume(corrected_last_length);
                debug!(" wrote {} bytes to temp file", corrected_last_length);
                buf_writer.flush()?;
                break;
            } else {
                buf_writer.write_all(&buffer)?;
                buffer_reader.consume(length);
            }
        }
    }

    // Open the file we are supposed to actuallly save to, and copy
    // content of temp. file to it. ensure we only write number of bytes that
    // Sublime Text has sent us.
    {
        // Move file cursor of temp. file to beginning.
        let rand_temp_file = &mut opened_buffers.get_mut(&token).unwrap().temp_file;
        rand_temp_file.seek(SeekFrom::Start(0))?;
    }

    if !opened_buffers.get(&token).unwrap().canwrite {
        debug!("File is read-only, not touching it!");
        return Ok(0);
    }

    debug!(
        "About to copy the temp file to actual one ({:?})",
        opened_buffers.get(&token).unwrap().display_name
    );
    opened_buffers
        .get_mut(&token)
        .ok_or(Error::new(
            ErrorKind::Other,
            "can't find the open buffer for saving",
        ))
        .and_then(|opened_buffer| {
            let fn_canon = opened_buffer.canon_path.as_path();
            let fp = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(fn_canon)
                .map_err(|e| {
                    Error::new(
                        ErrorKind::Other,
                        format!("{}: {:?}", fn_canon.to_string_lossy(), e.to_string()),
                    )
                })?;
            let mut temp_file = File::try_clone(&opened_buffer.temp_file)?;
            temp_file.seek(SeekFrom::Start(0))?;
            let temp_reader_sized = temp_file.take(data_size as u64);

            let mut buffer_writer = BufWriter::new(fp);
            let mut buffer_reader = BufReader::new(temp_reader_sized);
            let written_size =
                std::io::copy(&mut buffer_reader, &mut buffer_writer).map_err(|e| {
                    Error::new(
                        ErrorKind::Other,
                        format!("{}: {:?}", fn_canon.to_string_lossy(), e),
                    )
                })?;
            Ok((written_size, fn_canon))
        })
        .and_then(|(written_size, fn_canon)| {
            assert_eq!(data_size as u64, written_size);
            info!("Saved to {:?}", fn_canon);
            Ok(written_size as usize)
        })
}

// Check if file is writable by user
// metadata.permissions.readonly() checks all bits of file,
// regradless of which user is trying to write to it.
// So it seems actually trying to open the file in write mode is
// the only reliable way of checking the write access of current
// user in a cross platform manner
fn is_writable<P: AsRef<Path>>(p: P, md: &std::fs::Metadata) -> bool {
    !md.permissions().readonly() && OpenOptions::new().write(true).append(true).open(p).is_ok()
}

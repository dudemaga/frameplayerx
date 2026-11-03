//! The source and media crates' errors in the interface's language. The
//! crates word them in English for the log; what a server, a file or a
//! library said inside one is kept as it was said.

pub fn source(e: &fp_sources::Error) -> String {
    use fp_sources::Error as E;
    match e {
        E::Io(io) => tr!("error-io", error = io.to_string()),
        E::HttpStatus { status, url } => {
            tr!(
                "error-http-status",
                status = status.to_string(),
                url = url.as_str()
            )
        }
        E::Http { url, message } => tr!("error-http", url = url.as_str(), error = message.as_str()),
        E::Auth(server) => tr!("error-auth", server = server.as_str()),
        E::SignedOut(server) => tr!("error-signed-out", server = server.as_str()),
        E::NotFound(what) => tr!("error-not-found", what = what.as_str()),
        E::NotInserted => tr!("error-not-inserted").to_string(),
        E::InvalidLocation { location, reason } => tr!(
            "error-invalid-location",
            location = location.as_str(),
            reason = reason.as_str()
        ),
        E::Parse { what, message } => {
            tr!(
                "error-parse",
                what = what.as_str(),
                error = message.as_str()
            )
        }
        E::Unsupported(what) => tr!("error-unsupported", what = what.as_str()),
        E::Smb(message) => tr!("error-smb", error = message.as_str()),
        E::SmbUnavailable(message) => tr!("error-smb-unavailable", error = message.as_str()),
        E::Config(message) => tr!("error-config", error = message.as_str()),
    }
}

pub fn media(e: &fp_media::Error) -> String {
    use fp_media::Error as E;
    match e {
        E::Ffmpeg {
            context, message, ..
        } => {
            tr!(
                "error-ffmpeg",
                context = context.to_string(),
                error = message.as_str()
            )
        }
        E::NoStream("video") => tr!("error-no-video").to_string(),
        E::NoStream("audio") => tr!("error-no-audio").to_string(),
        E::NoStream(kind) => tr!("error-no-stream", kind = kind.to_string()),
        E::Unsupported(what) => tr!("error-unsupported", what = what.as_str()),
        E::Disc(message) => tr!("error-disc", error = message.as_str()),
        E::HardwareBusy(message, _) => tr!("error-hardware-busy", error = message.as_str()),
        E::HardwareTooBig(message) => tr!("error-hardware-too-big", error = message.as_str()),
        E::Audio(message) => tr!("error-audio", error = message.as_str()),
        E::Io(io) => tr!("error-io", error = io.to_string()),
        E::Interrupted => tr!("error-interrupted").to_string(),
    }
}

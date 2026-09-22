use std::{
    fs::File,
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    path::PathBuf,
};

use calloop::{LoopHandle, PostAction};
use filedescriptor::Pipe;
use strum::IntoEnumIterator;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::{
    GNOME_COPIED_FILES_MIME_TYPE, WaylandClientStatePtr, clipboard_item_from_paths, external_paths,
    parse_gnome_copied_files, parse_uri_list,
    platform::{PIPE_READ_TIMEOUT, read_fd_with_timeout},
    serialize_gnome_copied_files, serialize_uri_list,
};
use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat, hash};

/// Text mime types that we'll offer to other programs.
pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const FILE_LIST_MIME_TYPE: &str = "text/uri-list";

/// Text mime types that we'll accept from other programs.
pub(crate) const ALLOWED_TEXT_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "UTF8_STRING"];

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    self_mime: String,

    // Internal clipboard
    contents: Option<ClipboardItem>,
    primary_contents: Option<ClipboardItem>,

    // External clipboard
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<WlDataOffer>>,
    cached_primary_read: Option<ClipboardItem>,
    current_primary_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|t| t == mime_type)
    }

    fn read_bytes(&self, connection: &Connection, mime_type: &str) -> Option<Vec<u8>> {
        let pipe = Pipe::new().unwrap();
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        connection.flush().unwrap();

        match read_fd_with_timeout(fd, PIPE_READ_TIMEOUT) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                log::error!("error reading clipboard pipe: {err:?}");
                None
            }
        }
    }

    fn read_text(&self, connection: &Connection) -> Option<ClipboardItem> {
        let mime_type = self.mime_types.iter().find(|&mime_type| {
            ALLOWED_TEXT_MIME_TYPES
                .iter()
                .any(|&allowed| allowed == mime_type)
        })?;
        let bytes = self.read_bytes(connection, mime_type)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                log::error!("Failed to convert clipboard content to UTF-8: {}", e);
                return None;
            }
        };

        // Normalize the text to unix line endings, otherwise
        // copying from eg: firefox inserts a lot of blank
        // lines, and that is super annoying.
        let result = text_content.replace("\r\n", "\n");
        Some(ClipboardItem::new_string(result))
    }

    fn read_image(&self, connection: &Connection) -> Option<ClipboardItem> {
        for format in ImageFormat::iter() {
            let mime_type = format.mime_type();
            if !self.has_mime_type(mime_type) {
                continue;
            }

            if let Some(bytes) = self.read_bytes(connection, mime_type) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }

    fn read_files(&self, connection: &Connection) -> Option<ClipboardItem> {
        if self.has_mime_type(GNOME_COPIED_FILES_MIME_TYPE)
            && let Some(bytes) = self.read_bytes(connection, GNOME_COPIED_FILES_MIME_TYPE)
            && let Some((_is_cut, paths)) = parse_gnome_copied_files(&bytes)
            && !paths.is_empty()
        {
            return Some(clipboard_item_from_paths(paths));
        }
        if self.has_mime_type(FILE_LIST_MIME_TYPE)
            && let Some(bytes) = self.read_bytes(connection, FILE_LIST_MIME_TYPE)
        {
            let paths = parse_uri_list(&bytes);
            if !paths.is_empty() {
                return Some(clipboard_item_from_paths(paths));
            }
        }
        None
    }
}

impl Clipboard {
    pub fn new(
        connection: Connection,
        loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    ) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime: format!("pid/{}", std::process::id()),

            contents: None,
            primary_contents: None,

            cached_read: None,
            current_offer: None,
            cached_primary_read: None,
            current_primary_offer: None,
        }
    }

    pub fn set(&mut self, item: ClipboardItem) {
        self.contents = Some(item);
    }

    pub fn set_primary(&mut self, item: ClipboardItem) {
        self.primary_contents = Some(item);
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.cached_read = None;
        self.current_offer = data_offer;
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.cached_primary_read = None;
        self.current_primary_offer = data_offer;
    }

    pub fn self_mime(&self) -> String {
        self.self_mime.clone()
    }

    pub fn send(&self, mime_type: String, fd: OwnedFd) {
        if let Some(bytes) = contents_bytes_for_mime(self.contents.as_ref(), &mime_type) {
            self.send_bytes(fd, bytes);
        }
    }

    /// Answers a request for the files of a drag this window started. The drag
    /// carries paths rather than a clipboard item, so it does not go through the
    /// clipboard's own contents, but the writing is the same non-blocking write.
    pub fn send_paths(&self, mime_type: &str, paths: &[PathBuf], fd: OwnedFd) {
        self.send_bytes(fd, answered_with(mime_type, paths));
    }

    pub fn send_primary(&self, mime_type: String, fd: OwnedFd) {
        if let Some(bytes) = contents_bytes_for_mime(self.primary_contents.as_ref(), &mime_type) {
            self.send_bytes(fd, bytes);
        }
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_offer.as_ref()?;
        if let Some(cached) = self.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.contents.clone();
        }

        let item = offer
            .read_files(&self.connection)
            .or_else(|| offer.read_text(&self.connection))
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_primary_offer.as_ref()?;
        if let Some(cached) = self.cached_primary_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.primary_contents.clone();
        }

        let item = offer
            .read_files(&self.connection)
            .or_else(|| offer.read_text(&self.connection))
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_primary_read = Some(item.clone());
        Some(item)
    }

    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        let mut written = 0;
        self.loop_handle
            .insert_source(
                calloop::generic::Generic::new(
                    File::from(fd),
                    calloop::Interest::WRITE,
                    calloop::Mode::Level,
                ),
                move |_, file, _| {
                    let file = unsafe { file.get_mut() };
                    loop {
                        match file.write(&bytes[written..]) {
                            Ok(n) if written + n == bytes.len() => {
                                written += n;
                                break Ok(PostAction::Remove);
                            }
                            Ok(n) => written += n,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                break Ok(PostAction::Continue);
                            }
                            Err(_) => break Ok(PostAction::Remove),
                        }
                    }
                },
            )
            .unwrap();
    }
}

/// What a drag answers a request for `mime_type` with.
///
/// The GNOME format is a clipboard convention and carries its own `copy`/`cut`
/// verb, which a drag has no honest value for: the action is negotiated after
/// the data may already have been asked for. A file drag does not offer it --
/// see [`formats_a_file_drag_offers`] -- and a receiver that asks for it anyway
/// is told what the clipboard would say.
fn answered_with(mime_type: &str, paths: &[PathBuf]) -> Vec<u8> {
    match mime_type {
        GNOME_COPIED_FILES_MIME_TYPE => serialize_gnome_copied_files(paths, false).into_bytes(),
        FILE_LIST_MIME_TYPE => serialize_uri_list(paths).into_bytes(),
        _ => paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
    }
}

fn contents_bytes_for_mime(contents: Option<&ClipboardItem>, mime_type: &str) -> Option<Vec<u8>> {
    let contents = contents?;
    if mime_type == GNOME_COPIED_FILES_MIME_TYPE {
        let paths = external_paths(contents)?;
        return Some(serialize_gnome_copied_files(paths, false).into_bytes());
    }
    if mime_type == FILE_LIST_MIME_TYPE {
        let paths = external_paths(contents)?;
        return Some(serialize_uri_list(paths).into_bytes());
    }
    contents.text().map(String::into_bytes)
}

/// The formats a file drag puts on offer.
///
/// One, and deliberately: `text/uri-list` is the format a drag is read with,
/// and it carries paths and nothing else, so what happens to the original is
/// decided by the negotiated action alone. The GNOME clipboard format was
/// offered here too and carried its own `copy`/`cut` verb, which a drag cannot
/// fill in honestly -- the protocol lets a receiver ask for the data before the
/// action is settled, and lets it ask more than once -- so a second answer to
/// the same question could disagree with the first.
pub(crate) fn formats_a_file_drag_offers() -> [&'static str; 1] {
    [FILE_LIST_MIME_TYPE]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drag says what it carries and lets the action say what becomes of it.
    /// Offering the clipboard format here put a second, contradictable answer
    /// in the payload itself.
    #[test]
    fn a_file_drag_offers_the_drag_format_and_not_the_clipboard_one() {
        let offered = formats_a_file_drag_offers();

        assert!(
            offered.contains(&FILE_LIST_MIME_TYPE),
            "a drag is read as a uri list: {offered:?}"
        );
        assert!(
            !offered.contains(&GNOME_COPIED_FILES_MIME_TYPE),
            "and never as a clipboard verb: {offered:?}"
        );
    }

    /// Whatever a receiver asks for, the answer names the files. The uri list
    /// is the one that matters, and it is the same list however the drag ends.
    #[test]
    fn every_answer_names_the_files_it_carries() {
        let paths = [PathBuf::from("/tmp/one.txt"), PathBuf::from("/tmp/two.txt")];

        for mime_type in [
            FILE_LIST_MIME_TYPE,
            GNOME_COPIED_FILES_MIME_TYPE,
            "text/plain",
        ] {
            let said =
                String::from_utf8(answered_with(mime_type, &paths)).expect("the payload is text");
            assert!(
                said.contains("one.txt") && said.contains("two.txt"),
                "{mime_type} names both files: {said:?}"
            );
        }
    }
}

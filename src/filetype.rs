//! Extension → category classification. Pure lookup, no I/O.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    Folder = 0,
    Image = 1,
    Video = 2,
    Audio = 3,
    Document = 4,
    Archive = 5,
    Code = 6,
    App = 7,
    Other = 8,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Folder,
        Kind::Image,
        Kind::Video,
        Kind::Audio,
        Kind::Document,
        Kind::Archive,
        Kind::Code,
        Kind::App,
        Kind::Other,
    ];

    pub fn from_u8(v: u8) -> Kind {
        Kind::ALL.get(v as usize).copied().unwrap_or(Kind::Other)
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Folder => "Folders",
            Kind::Image => "Images",
            Kind::Video => "Videos",
            Kind::Audio => "Audio",
            Kind::Document => "Documents",
            Kind::Archive => "Archives",
            Kind::Code => "Code",
            Kind::App => "Apps",
            Kind::Other => "Other",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        let s = s.to_ascii_lowercase();
        Some(match s.as_str() {
            "folder" | "folders" | "dir" | "directory" => Kind::Folder,
            "image" | "images" | "img" | "photo" | "photos" | "picture" | "pic" => Kind::Image,
            "video" | "videos" | "movie" | "movies" | "vid" => Kind::Video,
            "audio" | "music" | "sound" | "song" | "songs" => Kind::Audio,
            "doc" | "docs" | "document" | "documents" | "text" => Kind::Document,
            "archive" | "archives" | "zip" | "compressed" => Kind::Archive,
            "code" | "source" | "src" => Kind::Code,
            "app" | "apps" | "application" | "exe" | "executable" | "binary" => Kind::App,
            "other" => Kind::Other,
            _ => return None,
        })
    }

    pub fn icon(self) -> &'static str {
        match self {
            Kind::Folder => "📁",
            Kind::Image => "🖼",
            Kind::Video => "🎞",
            Kind::Audio => "🎵",
            Kind::Document => "📄",
            Kind::Archive => "📦",
            Kind::Code => "⌨",
            Kind::App => "🚀",
            Kind::Other => "◻",
        }
    }

    pub fn is_media(self) -> bool {
        matches!(self, Kind::Image | Kind::Video | Kind::Audio)
    }
}

/// Classify by (lower-case) file name. Directories are classified by the caller,
/// except macOS bundles (`.app`) which we treat as apps.
pub fn classify(lower_name: &[u8], is_dir: bool) -> Kind {
    let ext = match memchr::memrchr(b'.', lower_name) {
        Some(i) if i + 1 < lower_name.len() => &lower_name[i + 1..],
        _ => return if is_dir { Kind::Folder } else { Kind::Other },
    };
    if is_dir {
        return if ext == b"app" { Kind::App } else { Kind::Folder };
    }
    classify_ext(ext)
}

pub fn classify_ext(ext: &[u8]) -> Kind {
    match ext {
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"webp" | b"bmp" | b"tif" | b"tiff" | b"heic"
        | b"heif" | b"avif" | b"svg" | b"ico" | b"raw" | b"cr2" | b"cr3" | b"nef" | b"arw"
        | b"dng" | b"orf" | b"rw2" | b"psd" | b"jxl" | b"icns" | b"xcf" => Kind::Image,
        b"mp4" | b"mov" | b"m4v" | b"mkv" | b"avi" | b"wmv" | b"flv" | b"webm" | b"mpg"
        | b"mpeg" | b"3gp" | b"mts" | b"m2ts" | b"vob" | b"ogv" => Kind::Video,
        b"mp3" | b"wav" | b"flac" | b"aac" | b"m4a" | b"ogg" | b"oga" | b"opus" | b"wma"
        | b"aiff" | b"aif" | b"alac" | b"mid" | b"midi" | b"caf" => Kind::Audio,
        b"pdf" | b"doc" | b"docx" | b"xls" | b"xlsx" | b"ppt" | b"pptx" | b"odt" | b"ods"
        | b"odp" | b"rtf" | b"txt" | b"md" | b"pages" | b"numbers" | b"key" | b"epub"
        | b"csv" | b"tex" | b"log" | b"mobi" => Kind::Document,
        b"zip" | b"rar" | b"7z" | b"tar" | b"gz" | b"tgz" | b"bz2" | b"xz" | b"zst" | b"lz4"
        | b"dmg" | b"iso" | b"img" | b"pkg" | b"deb" | b"rpm" | b"cab" | b"jar" | b"xip" => {
            Kind::Archive
        }
        b"rs" | b"c" | b"h" | b"cpp" | b"hpp" | b"cc" | b"py" | b"js" | b"ts" | b"tsx"
        | b"jsx" | b"go" | b"java" | b"kt" | b"swift" | b"m" | b"mm" | b"rb" | b"php" | b"cs"
        | b"sh" | b"zsh" | b"bash" | b"html" | b"css" | b"scss" | b"json" | b"toml"
        | b"yaml" | b"yml" | b"xml" | b"sql" | b"lua" | b"dart" | b"vue" | b"svelte"
        | b"zig" | b"ex" | b"exs" | b"hs" | b"scala" | b"ipynb" => Kind::Code,
        b"app" | b"exe" | b"msi" | b"bat" | b"cmd" | b"appimage" | b"apk" | b"ipa"
        | b"dll" | b"so" | b"dylib" | b"com" => Kind::App,
        _ => Kind::Other,
    }
}

/// Image formats our pure-Rust decoder can read (plus HEIC through `sips` on macOS).
pub fn is_decodable_image(lower_name: &str) -> bool {
    let ext = lower_name.rsplit('.').next().unwrap_or("");
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" => true,
        "heic" | "heif" => cfg!(target_os = "macos"),
        _ => false,
    }
}

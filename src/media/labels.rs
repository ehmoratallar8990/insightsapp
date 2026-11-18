//! Zero-shot label vocabulary. Labels are matched against image embeddings with
//! CLIP prompts; changing this list re-labels the library from stored embeddings
//! without re-running the vision model.

/// (label, prompt)
pub const LABELS: &[(&str, &str)] = &[
    // People
    ("person", "a photo of a person"),
    ("selfie", "a selfie photo of a face"),
    ("group", "a group photo of several people"),
    ("baby", "a photo of a baby"),
    ("child", "a photo of a child"),
    ("wedding", "a photo of a wedding"),
    ("party", "a photo of a party celebration"),
    ("concert", "a photo of a concert"),
    ("sports", "a photo of people playing sports"),
    // Animals
    ("dog", "a photo of a dog"),
    ("cat", "a photo of a cat"),
    ("bird", "a photo of a bird"),
    ("horse", "a photo of a horse"),
    ("fish", "a photo of fish"),
    ("insect", "a photo of an insect"),
    ("wildlife", "a photo of wild animals"),
    // Nature & places
    ("beach", "a photo of a beach"),
    ("ocean", "a photo of the ocean"),
    ("mountain", "a photo of mountains"),
    ("forest", "a photo of a forest"),
    ("lake", "a photo of a lake"),
    ("river", "a photo of a river"),
    ("desert", "a photo of a desert"),
    ("snow", "a photo of snow"),
    ("sunset", "a photo of a sunset"),
    ("sky", "a photo of the sky and clouds"),
    ("night", "a photo taken at night"),
    ("flowers", "a photo of flowers"),
    ("garden", "a photo of a garden"),
    ("park", "a photo of a park"),
    ("city", "a photo of a city skyline"),
    ("street", "a photo of a street"),
    ("building", "a photo of a building"),
    ("architecture", "a photo of architecture"),
    ("interior", "a photo of a room interior"),
    ("kitchen", "a photo of a kitchen"),
    ("bedroom", "a photo of a bedroom"),
    ("office", "a photo of an office"),
    ("restaurant", "a photo of a restaurant"),
    ("church", "a photo of a church"),
    ("landmark", "a photo of a famous landmark"),
    ("travel", "a travel photo"),
    ("airport", "a photo of an airport"),
    ("pool", "a photo of a swimming pool"),
    // Things
    ("food", "a photo of food"),
    ("drink", "a photo of a drink"),
    ("coffee", "a photo of coffee"),
    ("dessert", "a photo of a dessert"),
    ("car", "a photo of a car"),
    ("bicycle", "a photo of a bicycle"),
    ("motorcycle", "a photo of a motorcycle"),
    ("airplane", "a photo of an airplane"),
    ("boat", "a photo of a boat"),
    ("train", "a photo of a train"),
    ("phone", "a photo of a smartphone"),
    ("computer", "a photo of a computer"),
    ("keyboard", "a photo of a keyboard"),
    ("book", "a photo of a book"),
    ("clothing", "a photo of clothing"),
    ("shoes", "a photo of shoes"),
    ("furniture", "a photo of furniture"),
    ("toy", "a photo of a toy"),
    ("plant", "a photo of a house plant"),
    ("christmas", "a photo of christmas decorations"),
    ("fireworks", "a photo of fireworks"),
    // Digital content
    ("screenshot", "a screenshot of a computer screen"),
    ("phone screenshot", "a screenshot of a phone app"),
    ("code", "a screenshot of source code"),
    ("chart", "a chart or graph"),
    ("diagram", "a diagram"),
    ("map", "a map"),
    ("document", "a scanned document with text"),
    ("receipt", "a photo of a receipt"),
    ("invoice", "an invoice"),
    ("id card", "a photo of an id card or passport"),
    ("whiteboard", "a photo of a whiteboard"),
    ("handwriting", "handwritten notes"),
    ("text", "an image of text"),
    ("meme", "a meme"),
    ("logo", "a logo"),
    ("icon", "an app icon"),
    ("illustration", "a digital illustration"),
    ("drawing", "a drawing or sketch"),
    ("painting", "a painting"),
    ("cartoon", "a cartoon"),
    ("3d render", "a 3d render"),
    ("pattern", "an abstract pattern or texture"),
    ("wallpaper", "a desktop wallpaper"),
    ("ui design", "a user interface design mockup"),
    ("qr code", "a qr code"),
    ("art", "a work of art"),
    ("black and white", "a black and white photo"),
    ("blurry", "a blurry photo"),
    ("dark", "a very dark photo"),
];

pub fn prompts() -> Vec<String> {
    LABELS.iter().map(|(_, p)| p.to_string()).collect()
}

pub fn version_hash() -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for (l, p) in LABELS {
        h.update(l.as_bytes());
        h.update(p.as_bytes());
    }
    h.digest()
}

//! Generated default names for sessions, tabs, and panes.
//!
//! When a create command carries no explicit `--name`, the runtime asks this
//! module for one instead of prompting the user. A generated name is
//! `<TYPE>-<adjective>-<noun>`, where `TYPE` is a one-letter kind tag (`S`,
//! `T`, or `P`) and the adjective and noun come from the same language's
//! 50-entry word lists — English, Japanese, or Traditional Chinese — e.g.
//! `T-swift-otter`, `T-しずか-りす`, or `T-快樂-書房`. A name never mixes
//! languages. Names render at the UI layer (never inside a PTY grid), so the
//! mixed scripts carry no terminal-width constraint here.
//!
//! The pick is random: each call starts at a random combination and walks the
//! language x adjective x noun space in a coprime stride from there, returning
//! the first name the caller does not already hold — so a taken name is
//! skipped, never retried forever. Once every combination is taken, the walk
//! repeats with a numeric wrap suffix (`T-swift-otter-2`, then `-3`, and so
//! on), so a free name always exists.

use std::hash::{BuildHasher, Hasher, RandomState};

/// Which kind of entity a generated name labels; picks the name's one-letter
/// type tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    /// A session name, tagged `S`.
    Session,
    /// A tab name, tagged `T`.
    Tab,
    /// A pane name, tagged `P`.
    Pane,
}

impl NameKind {
    /// The one-letter tag a generated name of this kind starts with.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            NameKind::Session => "S",
            NameKind::Tab => "T",
            NameKind::Pane => "P",
        }
    }
}

/// English adjectives: exactly 50 entries.
pub const EN_ADJECTIVES: [&str; 50] = [
    "swift", "quiet", "brave", "calm", "bright", "gentle", "bold", "merry", "keen", "lucky",
    "misty", "golden", "silver", "wild", "cozy", "vivid", "noble", "amber", "breezy", "candid",
    "daring", "dusty", "eager", "fabled", "fierce", "frosty", "hearty", "humble", "jolly",
    "lively", "lofty", "mellow", "nimble", "plucky", "proud", "rosy", "rugged", "serene", "shiny",
    "sleek", "snug", "spry", "stark", "sturdy", "sunny", "tidy", "tranquil", "velvet", "witty",
    "zesty",
];

/// English nouns: exactly 50 entries.
pub const EN_NOUNS: [&str; 50] = [
    "otter", "maple", "ember", "river", "falcon", "harbor", "meadow", "comet", "pebble", "willow",
    "badger", "lantern", "tundra", "orchid", "walnut", "heron", "prairie", "acorn", "beacon",
    "canyon", "cedar", "clover", "coral", "crane", "dune", "fern", "fox", "garnet", "glacier",
    "grove", "hazel", "iris", "jasper", "kestrel", "lagoon", "linden", "lotus", "marble", "nettle",
    "oak", "opal", "osprey", "pine", "quartz", "raven", "reef", "sparrow", "thistle", "wren",
    "zephyr",
];

/// Japanese adjectives: exactly 50 entries.
pub const JA_ADJECTIVES: [&str; 50] = [
    "しずか",
    "はやい",
    "あかい",
    "あおい",
    "まるい",
    "ちいさい",
    "おおきい",
    "ふわふわ",
    "きらきら",
    "のんびり",
    "げんき",
    "かわいい",
    "つよい",
    "やさしい",
    "ひかる",
    "まぶしい",
    "さわやか",
    "あたたかい",
    "すずしい",
    "あまい",
    "あたらしい",
    "ふるい",
    "たかい",
    "ひろい",
    "ほそい",
    "まっしろ",
    "まっくろ",
    "こがね",
    "みどり",
    "あざやか",
    "おだやか",
    "なごやか",
    "ほがらか",
    "すこやか",
    "しなやか",
    "かろやか",
    "はなやか",
    "つややか",
    "さっぱり",
    "ゆったり",
    "きっちり",
    "すっきり",
    "ぽかぽか",
    "さらさら",
    "つるつる",
    "ぴかぴか",
    "にこにこ",
    "わくわく",
    "うきうき",
    "のびのび",
];

/// Japanese nouns: exactly 50 entries.
pub const JA_NOUNS: [&str; 50] = [
    "ねこ",
    "つき",
    "さくら",
    "うみ",
    "かぜ",
    "ほし",
    "やま",
    "もり",
    "ゆき",
    "そら",
    "かわうそ",
    "きつね",
    "たぬき",
    "すずめ",
    "くじら",
    "ひまわり",
    "もみじ",
    "いぬ",
    "うさぎ",
    "くま",
    "しか",
    "さる",
    "とり",
    "パンダ",
    "りす",
    "はな",
    "くさ",
    "たけ",
    "うめ",
    "まつ",
    "かえで",
    "いけ",
    "かわ",
    "たき",
    "くも",
    "あめ",
    "にじ",
    "ひかり",
    "みず",
    "いし",
    "すな",
    "はま",
    "しま",
    "たに",
    "おか",
    "はたけ",
    "こおり",
    "ほのお",
    "つばめ",
    "ふくろう",
];

/// Traditional Chinese adjectives: exactly 50 entries.
pub const ZH_HANT_ADJECTIVES: [&str; 50] = [
    "快樂", "安靜", "勇敢", "聰明", "溫柔", "明亮", "神秘", "悠閒", "燦爛", "可愛", "強壯", "輕盈",
    "靈巧", "沉穩", "活潑", "優雅", "潔白", "碧綠", "金黃", "銀白", "溫暖", "涼爽", "清新", "甜美",
    "嶄新", "古樸", "高遠", "遼闊", "細膩", "圓潤", "迅捷", "敏捷", "從容", "安然", "祥和", "開朗",
    "健壯", "柔軟", "輕快", "華麗", "光潤", "爽朗", "舒適", "寧靜", "晶瑩", "飄逸", "精緻", "樸實",
    "歡快", "自在",
];

/// Traditional Chinese nouns: exactly 50 entries.
pub const ZH_HANT_NOUNS: [&str; 50] = [
    "老虎",
    "熊貓",
    "竹子",
    "蓮花",
    "山丘",
    "海洋",
    "星辰",
    "月亮",
    "森林",
    "雲朵",
    "麻雀",
    "燈籠",
    "茶壺",
    "石頭",
    "稻田",
    "花園",
    "大象",
    "獅子",
    "猴子",
    "白鶴",
    "燕子",
    "蝴蝶",
    "鯉魚",
    "海豚",
    "貓頭鷹",
    "松樹",
    "梅花",
    "蘭花",
    "菊花",
    "楓葉",
    "柳樹",
    "桃花",
    "山谷",
    "溪流",
    "瀑布",
    "湖泊",
    "島嶼",
    "沙灘",
    "彩虹",
    "雷雨",
    "微風",
    "晨露",
    "夕陽",
    "星空",
    "燈塔",
    "城堡",
    "橋樑",
    "庭院",
    "書房",
    "茶館",
];

/// The per-language word lists, adjective list paired with noun list. A
/// generated name draws both words from one entry, so a name never mixes
/// languages.
const LANGUAGES: [(&[&str; 50], &[&str; 50]); 3] = [
    (&EN_ADJECTIVES, &EN_NOUNS),
    (&JA_ADJECTIVES, &JA_NOUNS),
    (&ZH_HANT_ADJECTIVES, &ZH_HANT_NOUNS),
];

/// Words per list: every language contributes exactly this many adjectives
/// and this many nouns.
const WORDS_PER_LIST: usize = 50;

/// Every candidate name: 3 languages x 50 adjectives x 50 nouns.
const COMBOS: usize = LANGUAGES.len() * WORDS_PER_LIST * WORDS_PER_LIST;

/// The step between consecutive candidates in the combination walk. Coprime
/// with [`COMBOS`], so one round from any start visits every combination
/// exactly once; `73 % 3 == 1` also moves the walk to the next language on
/// every step.
const STRIDE: usize = 73;

/// Generate a random default name of `kind` that `is_taken` does not already
/// claim.
///
/// The random start lands on a random language as well as a random word pair
/// — consecutive calls yield a mix of English, Japanese, and Traditional
/// Chinese names. The walk from that start skips taken names and appends a
/// wrap number once every combination is claimed — so the call always
/// returns a free name.
#[must_use]
pub fn generate_name(kind: NameKind, is_taken: impl Fn(&str) -> bool) -> String {
    generate_name_from(kind, is_taken, random_index(COMBOS))
}

/// Generate the first free name of `kind` walking the combination space from
/// `start`.
///
/// Visits every language x adjective x noun combination once per round in
/// [`STRIDE`] steps, returning the first `<TYPE>-<adjective>-<noun>` the
/// caller reports free. When a full round finds every combination taken,
/// subsequent rounds append a wrap number starting at `2`
/// (`T-swift-otter-2`), so the walk always terminates. The same `start` and
/// taken-set always yield the same name.
fn generate_name_from(kind: NameKind, is_taken: impl Fn(&str) -> bool, start: usize) -> String {
    let mut round: usize = 0;
    loop {
        for step in 0..COMBOS {
            let index = (start + step * STRIDE) % COMBOS;
            // The language is the index's low residue: the random start picks
            // one at random, and each stride step moves to the next.
            let (adjectives, nouns) = LANGUAGES[index % LANGUAGES.len()];
            let pair = index / LANGUAGES.len();
            let adjective = adjectives[pair / WORDS_PER_LIST];
            let noun = nouns[pair % WORDS_PER_LIST];
            let candidate = if round == 0 {
                format!("{}-{adjective}-{noun}", kind.prefix())
            } else {
                format!("{}-{adjective}-{noun}-{}", kind.prefix(), round + 1)
            };
            if !is_taken(&candidate) {
                return candidate;
            }
        }
        round += 1;
    }
}

/// A random index in `0..bound`, drawn from the standard library's randomly
/// seeded hasher state — each call builds a fresh [`RandomState`] and uses its
/// hash output as the entropy source, so no external RNG dependency is needed.
fn random_index(bound: usize) -> usize {
    let entropy = RandomState::new().build_hasher().finish();
    (entropy % bound as u64) as usize
}

#[cfg(test)]
mod tests;

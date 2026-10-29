//! String normalization and similarity for matching local tags against
//! MusicBrainz candidates.

const EDITION_SUFFIXES: [&str; 7] = [
    "deluxe edition",
    "remastered",
    "remaster",
    "bonus tracks",
    "edition",
    "deluxe",
    "expanded",
];

/// Lowercase, without bracketed asides, a leading "the", punctuation, or
/// trailing edition words, so "The Wall (Deluxe Edition)" and "Wall" agree.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    let mut words: Vec<&str> = out.split_whitespace().collect();
    if words.len() > 1 && words[0] == "the" {
        words.remove(0);
    }
    let mut text = words.join(" ");
    loop {
        let before = text.len();
        for suffix in EDITION_SUFFIXES {
            if let Some(rest) = text.strip_suffix(suffix) {
                let rest = rest.trim_end();
                if !rest.is_empty() && rest.len() < text.len() - suffix.len() {
                    text = rest.to_string();
                }
            }
        }
        if text.len() == before {
            break;
        }
    }
    text
}

/// Equal after lowercasing and dropping punctuation, nothing more. Used
/// where a near miss would fetch the wrong album's cover.
pub fn same_name(a: &str, b: &str) -> bool {
    let strict = |s: &str| {
        s.chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (a, b) = (strict(a), strict(b));
    !a.is_empty() && a == b
}

/// Normalized Levenshtein similarity in `[0, 1]`; 1.0 means equal after
/// [`normalize`].
pub fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = normalize(a).chars().collect();
    let b: Vec<char> = normalize(b).chars().collect();
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    1.0 - levenshtein(&a, &b) as f64 / longest as f64
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_articles_brackets_and_editions() {
        assert_eq!(normalize("The Wall (Deluxe Edition)"), "wall");
        assert_eq!(normalize("In Rainbows [Disc 1]"), "in rainbows");
        assert_eq!(normalize("OK Computer: Remastered"), "ok computer");
        assert_eq!(
            normalize("  Jogos de Azar: 6ª Rodada "),
            "jogos de azar 6ª rodada"
        );
        assert_eq!(normalize("The The"), "the");
        assert_eq!(normalize(""), "");
        assert_eq!(
            normalize("Expedition"),
            "expedition",
            "suffixes strip whole words only"
        );
        assert_eq!(normalize("Deluxe"), "deluxe");
    }

    #[test]
    fn same_name_ignores_case_and_punctuation_but_nothing_else() {
        assert!(same_name("OK Computer", "Ok Computer!"));
        assert!(same_name(
            "Jogos de Azar: 6ª Rodada",
            "Jogos de Azar 6ª Rodada"
        ));
        assert!(!same_name("Led Zeppelin II", "Led Zeppelin III"));
        assert!(!same_name("Greatest Hits Vol. 2", "Greatest Hits Vol. 1"));
        assert!(!same_name("Unplugged (Live)", "Unplugged"));
        assert!(!same_name("The Wall", "Wall"));
        assert!(!same_name("", "Wall"));
    }

    #[test]
    fn similarity_is_one_for_equal_and_low_for_unrelated() {
        assert_eq!(similarity("OK Computer", "Ok Computer!"), 1.0);
        assert!(similarity("OK Computer", "Kid A") < 0.4);
        assert!(similarity("Estranhos no Ninho", "Estranhos No Ninho (2002)") > 0.99);
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("abc", ""), 0.0);
    }
}

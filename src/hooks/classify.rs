pub struct Classifier(Vec<Vec<String>>);
impl Classifier {
    pub fn new(additions: &[String]) -> Self {
        Self(
            include_str!("heavy-commands.txt")
                .lines()
                .chain(additions.iter().map(String::as_str))
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| line.split_whitespace().map(str::to_owned).collect())
                .collect(),
        )
    }
    pub fn heavy(&self, command: &str) -> bool {
        let Some(segments) = segments(command) else {
            return false;
        };
        segments.iter().any(|words| {
            let mut words = words.as_slice();
            while words
                .first()
                .is_some_and(|word| assignment(word) || word == "env" || word == "command")
            {
                words = &words[1..];
            }
            let Some(program) = words.first() else {
                return false;
            };
            let program = program.rsplit('/').next().unwrap_or(program);
            self.0.iter().any(|pattern| {
                pattern[0] == program
                    && words.len() >= pattern.len()
                    && words[1..pattern.len()] == pattern[1..]
            })
        })
    }
}
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(key, _)| {
        !key.is_empty()
            && key
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    })
}
// ponytail: literal shell segments only; heredocs and shell -c bodies stay light.
// Use a full shell parser if classification of dynamic commands becomes necessary.
fn segments(command: &str) -> Option<Vec<Vec<String>>> {
    let mut result = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = command.chars().peekable();
    let mut active = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if quote != Some('\'') => {
                let escaped = chars.next()?;
                if escaped != '\n' {
                    word.push(escaped);
                    active = true;
                }
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(ch);
                active = true;
            }
            ch if quote == Some(ch) => quote = None,
            ch if quote.is_some() => word.push(ch),
            '<' if chars.peek() == Some(&'<') => return None,
            '#' if !active => {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
                if !words.is_empty() {
                    result.push(std::mem::take(&mut words));
                }
            }
            ';' | '|' | '&' | '\n' | '(' | ')' => {
                if active {
                    words.push(std::mem::take(&mut word));
                    active = false;
                }
                if !words.is_empty() {
                    result.push(std::mem::take(&mut words));
                }
            }
            ch if ch.is_whitespace() => {
                if active {
                    words.push(std::mem::take(&mut word));
                    active = false;
                }
            }
            ch => {
                word.push(ch);
                active = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if active {
        words.push(word);
    }
    if !words.is_empty() {
        result.push(words);
    }
    Some(result)
}

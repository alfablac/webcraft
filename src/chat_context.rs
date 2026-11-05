//! Pure multilingual local-context helpers (EN/PT-BR/ES). No I/O.
//! Integrator maps `LocalNeed` 1:1 onto `chat_core::LocalNeed`, fetches
//! geocode/weather itself, formats context. Standalone until wired in.

/// Mirror of `chat_core::LocalNeed` (same fields/semantics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalNeed {
    pub weather: bool,
    pub news: bool,
    pub location: bool,
}

/// Lowercase + strip common Latin accents: `áàâãä->a`, `ç->c`, `ñ->n`, etc.
/// ponytail: fixed accent table, no unicode-normalization dep; covers PT/ES.
fn normalize(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            'ý' | 'ÿ' => 'y',
            _ => c,
        })
        .collect()
}

fn has(t: &str, list: &[&str]) -> bool {
    list.iter().any(|k| t.contains(k))
}

/// EN+PT-BR+ES live-data need. News requires news word + local qualifier
/// (mirrors `chat_core::local_need`). Accent-insensitive via `normalize`.
pub fn local_need(text: &str) -> Option<LocalNeed> {
    let t = normalize(text);
    let has = |list: &[&str]| has(&t, list);
    let weather = has(&[
        // EN (mirror chat_core)
        "weather",
        "forecast",
        "forcast",
        "wether",
        "temperature",
        "temprature",
        "rain",
        "snow",
        "storm",
        "thunder",
        "hail",
        "umbrella",
        "jacket",
        "outside",
        "hot is it",
        "cold is it",
        "windy",
        "humid",
        "uv index",
        "air quality",
        "sunrise",
        "sunset",
        // PT-BR (normalized spellings match accented + unaccented input)
        "clima",
        "tempo",
        "previsao",
        "temperatura",
        "chuva",
        "chover",
        "vai chover",
        "neve",
        "tempestade",
        "umidade",
        "vento",
        "como esta o tempo",
        "como esta o clima",
        // ES
        "tiempo",
        "pronostico",
        "lluvia",
        "llover",
        "tormenta",
        "humedad",
        "viento",
        "va a llover",
    ]);
    let location = has(&[
        // EN
        "where am i",
        "my location",
        "current location",
        "city am i in",
        "country am i in",
        "state am i in",
        "region am i in",
        "timezone am i in",
        "time zone am i in",
        "my ip",
        "ip address",
        "my city",
        "my country",
        "my state",
        "my region",
        // PT-BR
        "onde estou",
        "onde eu estou",
        "minha localizacao",
        "localizacao atual",
        "em que cidade",
        "em que estado",
        "em que pais",
        "minha cidade",
        "meu estado",
        "meu pais",
        "meu ip",
        "endereco ip",
        // ES
        "donde estoy",
        "mi ubicacion",
        "ubicacion actual",
        "en que ciudad",
        "en que pais",
        "mi ciudad",
        "mi pais",
        "mi ip",
        "direccion ip",
    ]);
    let news_word = has(&[
        "news",
        "headlines",
        "headlins",
        "top stories",
        "current events",
        // PT-BR + ES share `noticias`
        "noticias",
        "manchetes",
        "titulares",
    ]);
    let local_qual = has(&[
        // EN
        "today",
        "latest",
        "near",
        "local",
        "here",
        "my area",
        "my city",
        "my country",
        "my region",
        "around me",
        "happening",
        // PT-BR
        "hoje",
        "agora",
        "aqui",
        "perto",
        "ultimas",
        "recentes",
        "minha cidade",
        "meu pais",
        "minha regiao",
        "perto de mim",
        "acontecendo",
        // ES
        "hoy",
        "ahora",
        "aqui",
        "cerca",
        "ultimas",
        "recientes",
        "mi ciudad",
        "mi pais",
        "cerca de mi",
    ]);
    let news = news_word && local_qual;
    if weather || news || location {
        Some(LocalNeed {
            weather,
            news,
            location,
        })
    } else {
        None
    }
}

/// Follow-up to a local-context answer, EN+PT+ES (`<80` chars, mirrors
/// `chat_core::is_local_followup`). Bare places handled by
/// `extract_named_place`, not here.
pub fn is_local_followup(text: &str) -> bool {
    let t = normalize(text.trim());
    [
        // EN
        "and tomorrow",
        "and tonight",
        "tomorrow",
        "tonight",
        "this weekend",
        "next week",
        "what about",
        "and then",
        "there",
        "near me",
        // PT-BR
        "e amanha",
        "amanha",
        "esta noite",
        "hoje a noite",
        "fim de semana",
        "proxima semana",
        "semana que vem",
        "e entao",
        "e hoje",
        "perto de mim",
        // ES
        "y manana",
        "manana",
        "esta noche",
        "fin de semana",
        "semana que viene",
        "cerca de mi",
    ]
    .iter()
    .any(|k| t.contains(k))
        && t.chars().count() < 80
}

const PLACE_STOP: &[&str] = &[
    "hoje",
    "agora",
    "amanha",
    "ontem",
    "today",
    "tomorrow",
    "tonight",
    "yesterday",
    "aqui",
    "here",
    "there",
    "ali",
    "la",
    "aca",
    "perto",
    "near",
    "nearby",
    "cerca",
    "please",
    "por",
    "favor",
    "obrigado",
    "obrigada",
    "gracias",
    "thanks",
    "clima",
    "tempo",
    "weather",
    "forecast",
    "noticias",
    "news",
    "temperatura",
    "temperature",
];
const PLACE_CONNECTORS: &[&str] = &["de", "da", "do", "das", "dos", "del", "e"];
const PLACE_GENERIC: &[&str] = &[
    "casa", "home", "work", "trabalho", "hoje", "agora", "aqui", "perto", "local",
];

fn clean_token(tok: &str) -> &str {
    tok.trim_matches(|c: char| matches!(c, '?' | '!' | ';' | '"' | '\'' | '(' | ')' | ':'))
}

/// Validate joined candidate: 1-4 words, <60 chars, letters/space/hyphen/
/// comma/apostrophe/dot only, no generic word, non-connector words
/// capitalized in original (ponytail: lowercase places rejected).
fn valid_place(candidate: &str) -> bool {
    let words: Vec<&str> = candidate.split_whitespace().collect();
    if words.is_empty() || words.len() > 4 || candidate.chars().count() >= 60 {
        return false;
    }
    if PLACE_GENERIC.contains(&normalize(candidate).as_str()) {
        return false;
    }
    words.iter().all(|w| {
        let w = w.trim_matches(|c: char| matches!(c, ',' | '.'));
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | '.' | ','))
            && (PLACE_CONNECTORS.contains(&normalize(w).as_str())
                || w.chars()
                    .find(|c| c.is_alphabetic())
                    .is_some_and(|c| c.is_uppercase()))
    })
}

/// Collect capitalized place tokens after token index `start`; stops at
/// stop-word, lowercase non-connector, or sentence punctuation.
fn collect_place(tokens: &[&str], start: usize) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for tok in &tokens[start..] {
        let end_sentence = tok.ends_with(['?', '!', ';']);
        let clean = clean_token(tok).trim_end_matches('.');
        let norm = normalize(clean.trim_end_matches(','));
        if PLACE_STOP.contains(&norm.as_str()) {
            break;
        }
        let connector = PLACE_CONNECTORS.contains(&norm.as_str());
        if !connector
            && !clean
                .trim_end_matches(',')
                .chars()
                .next()
                .is_some_and(|c| c.is_uppercase())
        {
            break;
        }
        parts.push(clean);
        if end_sentence || parts.len() > 4 {
            break;
        }
    }
    let candidate = parts.join(" ");
    let candidate = candidate.trim().trim_end_matches(',').to_string();
    valid_place(&candidate).then_some(candidate)
}

/// Heuristic place extraction for weather/clima follow-ups. Accepts bare
/// place (`Xique-Xique, Bahia`) or `em|para|in|de <Place>` patterns.
/// Rejects URLs, questions, generic words, >4 words, >=60 chars.
/// ponytail: capitalization heuristic, no gazetteer; lowercase places missed.
pub fn extract_named_place(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches(['?', '!', '.']);
    let norm = normalize(text);
    if text.is_empty()
        || norm.contains("http")
        || norm.contains("www.")
        || text.contains("://")
        || text.contains('/')
        || text.chars().any(|c| c.is_ascii_digit())
    {
        return None;
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    // Strong prepositions first.
    for (i, tok) in tokens.iter().enumerate() {
        if ["em", "para", "in"].contains(&normalize(clean_token(tok)).as_str()) {
            if let Some(p) = collect_place(&tokens, i + 1) {
                return Some(p);
            }
        }
    }
    // Bare place (reject keyword-led text so `de` pattern below still wins).
    // Token match only: substring would false-hit `xique` via `que`.
    let keyword_led = norm.split_whitespace().any(|w| {
        [
            "clima", "tempo", "weather", "noticias", "news", "como", "how", "qual", "que", "what",
        ]
        .contains(&w.trim_matches(|c: char| !c.is_alphabetic()))
    });
    if !keyword_led
        && tokens
            .first()
            .is_some_and(|f| !PLACE_CONNECTORS.contains(&normalize(clean_token(f)).as_str()))
        && valid_place(text)
    {
        return Some(text.to_string());
    }
    // Weak `de` pattern: `clima de São Paulo`, `notícias de Xique-Xique`.
    for (i, tok) in tokens.iter().enumerate() {
        if normalize(clean_token(tok)) == "de" {
            if let Some(p) = collect_place(&tokens, i + 1) {
                return Some(p);
            }
        }
    }
    None
}

/// application/x-www-form-urlencoded byte serializer (space `+`, keep
/// `A-Za-z0-9*-._`, else `%XX` uppercase). Self-contained; no `url` dep.
fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Web-search intent (EN/PT/ES). Explicit asks only: bare questions stay
/// local-only so app-help Q&A never triggers a fetch.
pub fn web_search_query(text: &str) -> Option<String> {
    let raw = text.trim();
    if raw.is_empty() || raw.contains("://") {
        return None;
    }
    let t = normalize(raw);
    let has = |list: &[&str]| has(&t, list);
    if !has(&[
        "search for",
        "search the web",
        "look up",
        "lookup",
        "google",
        "pesquise",
        "pesquisar",
        "procure por",
        "busque por",
        "buscar",
        "busca por",
        "busque",
        "investigue",
        "busca en la web",
        "buscar en",
        "investiga",
    ]) {
        return None;
    }
    // Query = original text minus leading command words; cap for URL sanity.
    let q: String = raw
        .trim_matches(|c: char| matches!(c, '?' | '!' | '.' | ' '))
        .chars()
        .take(200)
        .collect();
    (!q.trim().is_empty()).then_some(q)
}

/// First http(s) URL in text (page-capture intent), punctuation trimmed.
pub fn extract_url(text: &str) -> Option<String> {
    for tok in text.split_whitespace() {
        let tok = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | '<' | '>' | '(' | ')'));
        if let Some(pos) = tok.find("http://").or_else(|| tok.find("https://")) {
            let url =
                tok[pos..].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'']);
            if url.len() > 8 && !url.contains(char::is_whitespace) {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// DuckDuckGo HTML endpoint (no API key). Results parsed by caller.
pub fn ddg_search_url(query: &str) -> String {
    format!(
        "https://html.duckduckgo.com/html/?q={}",
        form_encode(query.trim())
    )
}

/// Parse DDG html results: (title, url) pairs from `result__a` anchors.
/// Decodes `uddg=` redirect param; plain hrefs kept as-is. Cap `max`.
pub fn parse_ddg_results(html: &str, max: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("result__a") {
        rest = &rest[pos..];
        let Some(href_start) = rest.find("href=\"") else {
            break;
        };
        rest = &rest[href_start + 6..];
        let Some(href_end) = rest.find('"') else {
            break;
        };
        let href = &rest[..href_end];
        let Some(gt) = rest.find('>') else { break };
        rest = &rest[gt + 1..];
        let Some(lt) = rest.find('<') else { break };
        let title = strip_tags(&rest[..lt]);
        let title = decode_entities(title.trim());
        let url = if let Some(u) = href.split("uddg=").nth(1) {
            percent_decode(u.split('&').next().unwrap_or(u))
        } else if href.starts_with("//") {
            format!("https:{href}")
        } else {
            href.to_string()
        };
        if !title.is_empty() && url.starts_with("http") && out.len() < max {
            out.push((title, url));
        }
        rest = &rest[lt..];
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
}

/// Crude HTML -> text: drop script/style, strip tags, collapse whitespace.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let tag_end = html[i..].find('>').map(|p| p + i);
            let Some(te) = tag_end else { break };
            let tag = &lower[i..te];
            if tag.starts_with("<script") || tag.starts_with("<style") {
                let close = if tag.starts_with("<script") {
                    "</script"
                } else {
                    "</style"
                };
                match lower[te..].find(close) {
                    Some(p) => {
                        // Skip past the closing tag's '>' as well.
                        let after = te + p + close.len();
                        i = html[after..]
                            .find('>')
                            .map(|g| after + g + 1)
                            .unwrap_or(html.len());
                        continue;
                    }
                    None => break,
                }
            }
            out.push(' ');
            i = te + 1;
        } else {
            // Push one UTF-8 char.
            let ch_len = html[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&html[i..i + ch_len]);
            i += ch_len;
        }
    }
    decode_entities(&out.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Open-Meteo geocoding search URL for `place` (count=1, pt).
pub fn geocode_url(place: &str) -> String {
    format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=pt&format=json",
        form_encode(place.trim())
    )
}

/// Parse geocoding response body: `results[0]` -> `(lat, lon, display)`.
/// Display joins non-empty name/admin1/country with `, `. Rejects bad
/// JSON, missing/empty results, empty name, non-finite/out-of-range or
/// `0,0` coords.
pub fn parse_geocode(json: &str) -> Option<(f64, f64, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let first = v.get("results")?.as_array()?.first()?;
    let lat = first.get("latitude")?.as_f64()?;
    let lon = first.get("longitude")?.as_f64()?;
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
        || (lat == 0.0 && lon == 0.0)
    {
        return None;
    }
    let part = |k: &str| {
        first
            .get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let name = part("name");
    if name.is_empty() {
        return None;
    }
    let display = [name, part("admin1"), part("country")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    Some((lat, lon, display))
}

/// Open-Meteo forecast URL via lat/lon: current temp/humidity/code/wind,
/// daily max/min/precip, 3 days, `timezone=auto`. Mirrors `chat_core`.
pub fn weather_url(lat: f64, lon: f64) -> String {
    format!("https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m&daily=temperature_2m_max,temperature_2m_min,precipitation_probability_max&forecast_days=3&timezone=auto")
}

/// Newest turns within `char_budget` (text char count), last always kept.
/// `budget==0` -> just last. If older turns dropped, prepends synthetic
/// `(false, "[Conversation summary — earlier turns omitted]\n- user: ...")`
/// with up to 6 bullets, first 100 chars per dropped user (`true`) message.
/// Nothing dropped -> history unchanged. Empty -> empty.
/// ponytail: summary is extractive + not budget-counted; no LLM summarizer.
pub fn build_window(history: &[(bool, String)], char_budget: usize) -> Vec<(bool, String)> {
    let Some(last) = history.last() else {
        return Vec::new();
    };
    if char_budget == 0 {
        return vec![last.clone()];
    }
    let mut used = 0usize;
    let mut start = history.len();
    for (user, text) in history.iter().rev() {
        let n = text.chars().count();
        if start < history.len() && used + n > char_budget {
            break;
        }
        used += n;
        start -= 1;
        let _ = user;
    }
    if start == 0 {
        return history.to_vec();
    }
    let dropped = &history[..start];
    let mut summary = "[Conversation summary — earlier turns omitted]".to_string();
    for (_, text) in dropped.iter().filter(|(user, _)| *user).take(6) {
        let line: String = text
            .chars()
            .map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c })
            .take(100)
            .collect();
        summary.push_str(&format!("\n- user: {}", line.trim_end()));
    }
    let mut out = vec![(false, summary)];
    out.extend_from_slice(&history[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pt_weather_detected() {
        let n = local_need("Como está o clima hoje?").expect("need");
        assert!(n.weather);
        assert!(local_need("vai chover amanhã?").unwrap().weather);
        assert!(local_need("previsão do tempo").unwrap().weather);
        assert!(local_need("temperatura agora").unwrap().weather);
    }

    #[test]
    fn pt_news_detected() {
        assert!(local_need("notícias de hoje aqui").unwrap().news);
        assert!(local_need("últimas notícias").unwrap().news);
        assert!(local_need("manchetes de hoje").unwrap().news);
        // News word without local qualifier: no need (mirrors EN rule).
        assert_eq!(local_need("notícias sobre linguagem rust"), None);
    }

    #[test]
    fn pt_location_detected() {
        assert!(local_need("onde estou?").unwrap().location);
        assert!(local_need("qual é a minha localização?").unwrap().location);
        assert!(local_need("qual é o meu ip?").unwrap().location);
    }

    #[test]
    fn es_weather_detected() {
        assert!(local_need("¿Cómo está el clima hoy?").unwrap().weather);
        assert!(local_need("pronóstico del tiempo").unwrap().weather);
        assert!(local_need("¿va a llover mañana?").unwrap().weather);
        assert!(local_need("¿dónde estoy?").unwrap().location);
        assert!(local_need("noticias de hoy aquí").unwrap().news);
    }

    #[test]
    fn en_regressions() {
        assert!(local_need("weather today?").unwrap().weather);
        assert!(local_need("local news today").unwrap().news);
        assert!(local_need("where am I?").unwrap().location);
        assert_eq!(local_need("news about rust language"), None);
        assert_eq!(local_need("how do queues work?"), None);
    }

    #[test]
    fn followup_pt_es_en() {
        assert!(is_local_followup("and tomorrow?"));
        assert!(is_local_followup("e amanhã?"));
        assert!(is_local_followup("¿y mañana?"));
        assert!(!is_local_followup("how do queues work?"));
    }

    #[test]
    fn place_extraction_accepts() {
        assert_eq!(
            extract_named_place("Xique-Xique, Bahia"),
            Some("Xique-Xique, Bahia".into())
        );
        assert_eq!(
            extract_named_place("Como está o clima em Xique-Xique, Bahia?"),
            Some("Xique-Xique, Bahia".into())
        );
        assert_eq!(
            extract_named_place("weather in São Paulo today"),
            Some("São Paulo".into())
        );
        assert_eq!(
            extract_named_place("e para Xique-Xique?"),
            Some("Xique-Xique".into())
        );
        assert_eq!(
            extract_named_place("clima de São Paulo"),
            Some("São Paulo".into())
        );
        assert_eq!(
            extract_named_place("Rio de Janeiro"),
            Some("Rio de Janeiro".into())
        );
    }

    #[test]
    fn place_extraction_rejects() {
        for bad in [
            "",
            "hoje",
            "como está o clima hoje?",
            "how do queues work?",
            "see https://example.com/SaoPaulo",
            "em casa",
            "weather in 2024",
            "Aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(extract_named_place(bad), None, "{bad}");
        }
    }

    #[test]
    fn web_search_and_url_and_ddg_parse() {
        assert!(web_search_query("search for rust egui release notes").is_some());
        assert!(web_search_query("pesquise a cotação do dólar hoje").is_some());
        assert!(web_search_query("buscar en la web noticias rust").is_some());
        assert_eq!(web_search_query("how do queues work?"), None);
        assert_eq!(web_search_query("search https://example.com/x"), None);
        assert_eq!(
            extract_url("leia https://example.com/artigo?id=1, por favor"),
            Some("https://example.com/artigo?id=1".into())
        );
        assert_eq!(extract_url("sem link aqui"), None);
        assert!(ddg_search_url("a b").contains("q=a+b"));
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=zz">Title &amp; One</a>
        <a class="result__a" href="https://rust-lang.org">Rust</a>"#;
        let r = parse_ddg_results(html, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            ("Title & One".into(), "https://example.com/a?x=1".into())
        );
        assert_eq!(r[1].1, "https://rust-lang.org");
        let txt =
            strip_tags("<style>x{}</style><p>Olá <b>mundo</b></p><script>evil()</script> fim");
        assert_eq!(txt, "Olá mundo fim");
    }

    #[test]
    fn geocode_parse_ok_and_bad() {
        let ok = r#"{"results":[{"name":"Xique-Xique","admin1":"Bahia","country":"Brazil","latitude":-10.82,"longitude":-42.73}]}"#;
        assert_eq!(
            parse_geocode(ok),
            Some((-10.82, -42.73, "Xique-Xique, Bahia, Brazil".into()))
        );
        for bad in [
            "not json",
            "{}",
            r#"{"results":[]}"#,
            r#"{"results":[{"name":"X","latitude":0.0,"longitude":0.0}]}"#,
            r#"{"results":[{"name":"","latitude":1.0,"longitude":2.0}]}"#,
            r#"{"results":[{"name":"X","latitude":91.0,"longitude":2.0}]}"#,
        ] {
            assert_eq!(parse_geocode(bad), None, "{bad}");
        }
    }

    #[test]
    fn geocode_url_encodes_space_and_accents() {
        assert_eq!(
            geocode_url("São Paulo"),
            "https://geocoding-api.open-meteo.com/v1/search?name=S%C3%A3o+Paulo&count=1&language=pt&format=json"
        );
        assert!(geocode_url("Xique-Xique, Bahia").contains("name=Xique-Xique%2C+Bahia&"));
    }

    #[test]
    fn weather_url_builds_open_meteo_forecast() {
        let u = weather_url(-10.82, -42.73);
        assert!(u.starts_with(
            "https://api.open-meteo.com/v1/forecast?latitude=-10.82&longitude=-42.73&"
        ));
        assert!(u.contains("temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m"));
        assert!(u.contains("forecast_days=3&timezone=auto"));
    }

    #[test]
    fn build_window_no_drop_identity() {
        let h = vec![(true, "hello".to_string()), (false, "hi there".to_string())];
        assert_eq!(build_window(&h, 10_000), h);
        assert_eq!(build_window(&[], 10), Vec::<(bool, String)>::new());
    }

    #[test]
    fn build_window_drop_keeps_last_and_summarizes() {
        let h = vec![
            (true, "alpha topic one".to_string()),
            (false, "answer one".to_string()),
            (true, "beta topic two".to_string()),
            (false, "answer two".to_string()),
            (true, "final ask".to_string()),
        ];
        let w = build_window(&h, 19);
        assert_eq!(w.len(), 3);
        assert!(!w[0].0);
        assert!(w[0]
            .1
            .starts_with("[Conversation summary — earlier turns omitted]\n"));
        assert!(w[0].1.contains("- user: alpha topic one"));
        assert!(w[0].1.contains("- user: beta topic two"));
        assert_eq!(w.last().unwrap().1, "final ask");
    }

    #[test]
    fn build_window_budget_zero_keeps_last() {
        let h = vec![
            (true, "old".to_string()),
            (false, "older answer".to_string()),
            (true, "last one".to_string()),
        ];
        assert_eq!(build_window(&h, 0), vec![(true, "last one".to_string())]);
        // Oversized last message still kept under tiny budget.
        assert_eq!(build_window(&h, 1).last().unwrap().1, "last one");
    }
}

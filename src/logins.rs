//! Login metadata and domain boundaries. Cookie values are never a saved
//! record, a public reply, or an error's detail.
use crate::access::Rule;
use crate::error::{Result, invalid, denied};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub fn site(input: &str) -> Result<String> {
    let host = input.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.len() > 253 || host.split('.').any(|s| s.is_empty() || s.len()>63 || s.starts_with('-') || s.ends_with('-') || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b==b'-')) {
        return Err(invalid("Give a site name such as irs.gov, without a path or scheme."));
    }
    let domain = psl::domain_str(&host).ok_or_else(|| invalid("Give a registrable site name."))?;
    Ok(domain.to_string())
}

/// Google sessions are established independently on each machine. This guard
/// is used before source export and receiver import, including federated routes.
pub fn independent_site(name: &str) -> bool {
    site(name).is_ok_and(|s| matches!(s.as_str(), "google.com" | "gmail.com"))
}

/// What a site did with the last login shared for it: `worked` or
/// `site_rejected`, with when (Unix milliseconds).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteMemory {
    pub result: String,
    pub at_ms: i64,
}

/// Login rules per site, for each computer and for `all` of them, and what
/// each site did with the last login shared for it.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRules {
    pub rules: BTreeMap<String, BTreeMap<String, Rule>>,
    #[serde(default, deserialize_with = "readable_memory")]
    pub memory: BTreeMap<String, SiteMemory>,
}

/// Site memory as saved; an entry from before `{result, at_ms}` is dropped.
fn readable_memory<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, SiteMemory>, D::Error> {
    let raw = BTreeMap::<String, Value>::deserialize(deserializer)?;
    Ok(raw.into_iter().filter_map(|(site, value)| Some((site, serde_json::from_value(value).ok()?))).collect())
}

impl LoginRules {
    /// The rule for `site` on `computer`: deny > ask > allow across the
    /// `all` layer and the computer's own; a site with no rule is Ask.
    pub fn rule(&self, site: &str, computer: &str) -> Rule {
        self.rules.get(site).into_iter().flat_map(|r| [r.get("all"), r.get(computer)]).flatten().copied().max().unwrap_or(Rule::Ask)
    }

    pub fn set(&mut self, name: &str, computer: &str, rule: Rule) -> Result<()> {
        let name = site(name)?;
        if self.rules.len() >= 2048 && !self.rules.contains_key(&name) {
            return Err(invalid("Login rule limit reached."));
        }
        self.rules.entry(name).or_default().insert(computer.to_string(), rule);
        Ok(())
    }

    /// What `computer` is told: each site with a rule for it or for all
    /// computers, or with a remembered result, as `{rule, last_result?}`.
    pub fn projection(&self, computer: &str) -> Value {
        let ruled = self.rules.iter().filter(|(_, layers)| layers.contains_key(computer) || layers.contains_key("all")).map(|(s, _)| s);
        let mut sites: Vec<&String> = ruled.chain(self.memory.keys()).collect();
        sites.sort();
        sites.dedup();
        let mut rows = Map::new();
        for name in sites {
            let mut row = json!({"rule": self.rule(name, computer)});
            if let Some(memory) = self.memory.get(name) {
                row["last_result"] = json!(memory);
            }
            rows.insert(name.clone(), row);
        }
        Value::Object(rows)
    }
}

/// Validate the whole bundle before the first write. Never include a bad
/// field's contents in an error; browser errors are also kept generic.
pub fn check_cookies(name: &str, cookies: &Value) -> Result<usize> {
    let name = site(name)?;
    let rows=cookies.as_array().filter(|a|a.len()<=2000).ok_or_else(||invalid("Invalid login cookie bundle."))?;
    for row in rows {
        let host = row["domain"].as_str().ok_or_else(||invalid("Invalid login cookie metadata."))?.trim_start_matches('.');
        if site(host).ok().as_deref()!=Some(name.as_str()) || row["value"].as_str().is_none() || row["name"].as_str().is_none() || row["path"].as_str().is_none_or(|s| !s.starts_with('/')) {
            return Err(invalid("A cookie is outside the approved site or has invalid metadata."));
        }
        if row.as_object().is_none_or(|m|m.keys().any(|k|!matches!(k.as_str(),"domain"|"hostOnly"|"httpOnly"|"name"|"path"|"sameSite"|"secure"|"session"|"storeId"|"value"|"expirationDate"|"partitionKey"))) {
            return Err(invalid("Invalid login cookie metadata."));
        }
        if ["hostOnly","httpOnly","secure","session"].iter().any(|k|!row[*k].is_boolean())
            || !matches!(row["sameSite"].as_str(),Some("unspecified"|"lax"|"strict"|"no_restriction"))
            || (row["sameSite"]=="no_restriction" && row["secure"]!=true)
            || (row["session"]==false && row["expirationDate"].as_f64().is_none_or(|n|!n.is_finite()||n<=0.0)) {
            return Err(invalid("Invalid login cookie flags or expiry."));
        }
        let cookie_name=row["name"].as_str().unwrap();
        if (cookie_name.starts_with("__Secure-") && row["secure"]!=true)
            || (cookie_name.starts_with("__Host-") && (row["secure"]!=true || row["hostOnly"]!=true || row["path"]!="/")) {
            return Err(invalid("Invalid login cookie prefix."));
        }
        if let Some(key)=row.get("partitionKey") {
            let origin=key["topLevelSite"].as_str().unwrap_or("");
            let host=origin.strip_prefix("https://").or_else(||origin.strip_prefix("http://"));
            if key.as_object().is_none_or(|m|m.keys().any(|k|!matches!(k.as_str(),"topLevelSite"|"hasCrossSiteAncestor")))
                || host.is_none_or(|h|site(h).is_err()) || !key["hasCrossSiteAncestor"].is_boolean() || row["secure"]!=true {
                return Err(invalid("Invalid login cookie partition."));
            }
        }
    }
    Ok(rows.len())
}

/// Managed policy applies to every profile. Until profile-specific enrollment
/// is available, fail closed unless the selected browser has only Default.
pub fn profile_root(browser: &str, home: &std::path::Path) -> Result<std::path::PathBuf> {
    let relative=match browser {
        "chromium"=>".config/chromium", "chrome"=>".config/google-chrome",
        "brave"=>".config/BraveSoftware/Brave-Browser", "brave-origin"=>".config/BraveSoftware/Brave-Origin",
        _=>return Err(invalid("Choose a supported browser.")),
    };
    Ok(home.join(relative))
}
pub fn check_profile(root: &std::path::Path) -> Result<()> {
    let state: Value=std::fs::read(root.join("Local State")).ok().and_then(|b|serde_json::from_slice(&b).ok()).ok_or_else(||denied("The selected browser profile is unavailable."))?;
    let profiles=state["profile"]["info_cache"].as_object().ok_or_else(||denied("The selected browser profile is unavailable."))?;
    if profiles.len()!=1 || !profiles.contains_key("Default") {return Err(denied("Login sharing requires a browser with only the selected Default profile."));}
    Ok(())
}

//! A count of what the title's shader programs hold, kept while the title runs.
//!
//! Every distinct program a title makes a pipeline with is read and decoded by
//! `riptide-shader`, and what did and did not decode is counted. The counts go to the
//! session log as the number of programs passes 1, 10, 100 and so on. This is how the
//! decoder is checked against a real title: nothing of the title's is kept but counts and
//! instruction names.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::trace;

#[derive(Default)]
struct Census {
    /// Hashes of the programs counted so far, so that each is counted once.
    seen: HashSet<u64>,
    programs: u64,
    /// Programs that could not be read, by the reason.
    unread: HashMap<String, u64>,
    code_words: u64,
    instructions: u64,
    unknown: u64,
    /// Programs whose walk met a branch out of the code or ran off its end.
    astray: u64,
    stages: BTreeMap<String, u64>,
    mnemonics: BTreeMap<&'static str, u64>,
    /// The first few instructions that did not decode, as text.
    unknown_samples: Vec<String>,
}

static CENSUS: Mutex<Option<Census>> = Mutex::new(None);

/// Counts one program, if it has not been counted before.
pub(crate) fn count(code: &[u8]) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    let hash = hasher.finish();
    let mut census = CENSUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let census = census.get_or_insert_with(Census::default);
    if !census.seen.insert(hash) {
        return;
    }
    census.programs += 1;
    match riptide_shader::summarize(code) {
        Ok(summary) => {
            census.code_words += summary.code_words as u64;
            census.instructions += summary.instructions as u64;
            census.unknown += summary.unknown as u64;
            for program in &summary.programs {
                *census
                    .stages
                    .entry(program.stage.short_name().to_owned())
                    .or_default() += 1;
                if program.stray_branches != 0 || program.runs_off_end {
                    census.astray += 1;
                }
            }
            for (name, times) in &summary.mnemonics {
                *census.mnemonics.entry(name).or_default() += u64::from(*times);
            }
            for sample in &summary.unknown_samples {
                if census.unknown_samples.len() < 12 {
                    census.unknown_samples.push(format!(
                        "{} opcode {:#x} word {:#010x}",
                        sample.format.name(),
                        sample.opcode,
                        sample.word
                    ));
                }
            }
        }
        Err(error) => {
            *census.unread.entry(format!("{error:?}")).or_default() += 1;
        }
    }
    let programs = census.programs;
    if programs == 1 || (programs >= 10 && is_power_of_ten(programs)) {
        report(census);
    }
}

fn is_power_of_ten(mut number: u64) -> bool {
    while number >= 10 && number.is_multiple_of(10) {
        number /= 10;
    }
    number == 1
}

fn report(census: &Census) {
    let mut most: Vec<(&&str, &u64)> = census.mnemonics.iter().collect();
    most.sort_by(|a, b| b.1.cmp(a.1));
    let most: Vec<String> = most
        .iter()
        .take(24)
        .map(|(name, times)| format!("{name} {times}"))
        .collect();
    trace!(
        "shader census: {} programs, {} not read {:?}; stages {:?}; {} code words, {} \
         instructions, {} not decoded, {} programs with a branch astray; {} distinct \
         instructions, most used: {}",
        census.programs,
        census.unread.values().sum::<u64>(),
        census.unread,
        census.stages,
        census.code_words,
        census.instructions,
        census.unknown,
        census.astray,
        census.mnemonics.len(),
        most.join(", ")
    );
    if !census.unknown_samples.is_empty() {
        trace!("shader census: not decoded: {:?}", census.unknown_samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_reported_at_one_ten_a_hundred() {
        for (number, expected) in [(1, true), (10, true), (100, true), (1000, true)] {
            assert_eq!(is_power_of_ten(number), expected, "{number}");
        }
        for number in [0, 2, 11, 20, 110, 999] {
            assert!(!is_power_of_ten(number), "{number}");
        }
    }

    #[test]
    fn a_program_that_is_no_container_is_counted_as_not_read_and_only_once() {
        count(b"not a shader program, test input A");
        count(b"not a shader program, test input A");
        let census = CENSUS.lock().unwrap();
        let census = census.as_ref().unwrap();
        assert!(census.programs >= 1);
        assert!(census.unread.values().sum::<u64>() >= 1);
    }
}

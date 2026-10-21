//! Pure cgroup event accounting, also tested on hosts without cgroup v2.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MemoryEvents {
    pub(super) max: u64,
    pub(super) oom: u64,
    pub(super) oom_kill: u64,
}

impl MemoryEvents {
    pub(super) fn parse(text: &str) -> Result<Self, &'static str> {
        let (mut max, mut oom, mut oom_kill) = (None, None, None);
        for line in text.lines() {
            let mut fields = line.split_ascii_whitespace();
            let key = fields.next().ok_or("invalid cgroup memory event counter")?;
            let value = fields
                .next()
                .ok_or("invalid cgroup memory event counter")?
                .parse::<u64>()
                .map_err(|_| "invalid cgroup memory event counter")?;
            match key {
                "max" => max = Some(value),
                "oom" => oom = Some(value),
                "oom_kill" => oom_kill = Some(value),
                _ => {}
            }
        }
        Ok(Self {
            max: max.ok_or("cgroup max event counter was absent")?,
            oom: oom.ok_or("cgroup oom event counter was absent")?,
            oom_kill: oom_kill.ok_or("cgroup oom_kill event counter was absent")?,
        })
    }

    /// A private aggregate domain can reach its limit before the worker leaf.
    /// Only events since this worker's admission count; a global OOM victim
    /// count alone does not prove either configured limit caused termination.
    pub(super) fn classify(
        self,
        domain_before: Self,
        domain_after: Self,
    ) -> Result<(bool, bool), &'static str> {
        let domain_max = domain_after
            .max
            .checked_sub(domain_before.max)
            .ok_or("private cgroup max event counter decreased")?;
        let domain_oom = domain_after
            .oom
            .checked_sub(domain_before.oom)
            .ok_or("private cgroup oom event counter decreased")?;
        domain_after
            .oom_kill
            .checked_sub(domain_before.oom_kill)
            .ok_or("private cgroup oom_kill event counter decreased")?;
        Ok((
            self.max != 0 || self.oom != 0 || domain_max != 0 || domain_oom != 0,
            self.oom_kill != 0 && (self.oom != 0 || domain_oom != 0),
        ))
    }

    pub(super) fn json(self) -> String {
        format!(
            "{{\"max\":{},\"oom\":{},\"oom_kill\":{}}}",
            self.max, self.oom, self.oom_kill
        )
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryEvents as E;

    #[test]
    fn private_parent_limit_and_leaf_victim_are_correlated() {
        let leaf = E {
            oom_kill: 2,
            ..E::default()
        };
        let before = E {
            max: 10,
            oom: 2,
            oom_kill: 3,
        };
        let after = E {
            max: 12,
            oom: 3,
            oom_kill: 5,
        };
        assert_eq!(leaf.classify(before, after), Ok((true, true)));
        assert_eq!(E::default().classify(before, after), Ok((true, false)));
        let max_only = E { max: 12, ..before };
        assert_eq!(leaf.classify(before, max_only), Ok((true, false)));
    }

    #[test]
    fn old_parent_events_and_global_oom_do_not_attribute_new_exit() {
        let old = E {
            max: 12,
            oom: 3,
            oom_kill: 5,
        };
        let leaf = E {
            oom_kill: 1,
            ..E::default()
        };
        assert_eq!(leaf.classify(old, old), Ok((false, false)));
        let global_oom = E { oom_kill: 6, ..old };
        assert_eq!(leaf.classify(old, global_oom), Ok((false, false)));
        assert_eq!(E::default().classify(old, old), Ok((false, false)));
        let local = E {
            max: 1,
            oom: 1,
            oom_kill: 1,
        };
        assert_eq!(local.classify(old, old), Ok((true, true)));
    }

    #[test]
    fn event_parser_and_counter_reset_fail_explicitly() {
        let value =
            E::parse("low 0\nhigh 0\nmax 10\noom 2\noom_kill 3\noom_group_kill 1\n").unwrap();
        assert_eq!(value.json(), "{\"max\":10,\"oom\":2,\"oom_kill\":3}");
        assert!(E::parse("max 0\noom 0\n").is_err());
        assert!(E::parse("max -1\noom 0\noom_kill 0\n").is_err());
        assert!(E::parse("max 0\noom 0\noom_kill 0\nunknown bad\n").is_err());
        for after in [
            E { max: 9, ..value },
            E { oom: 1, ..value },
            E {
                oom_kill: 2,
                ..value
            },
        ] {
            assert!(E::default().classify(value, after).is_err());
        }
    }
}

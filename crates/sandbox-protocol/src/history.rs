//! Guest-local retirement barriers. A caller must durably retain outcomes and
//! retire external consumers before requesting one; this type grants no authority.
use crate::{Id, OperationId, guest_model::Context};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    Commands,
    Files,
}

/// Closed inclusive prefix in opaque operation-ID order, not a timestamp/TTL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Barrier {
    pub version: u32,
    pub context: Context,
    pub domain: Domain,
    pub through: OperationId,
}
impl Barrier {
    pub fn validate(&self, context: &Context, domain: Domain) -> Result<()> {
        ensure!(
            self.version == 1 && self.domain == domain && &self.context == context,
            "history barrier ownership mismatch"
        );
        ensure!(
            context.generation > 0 && !context.boot_id.is_empty() && context.boot_id.len() <= 64,
            "invalid history context"
        );
        ensure!(
            self.through.uuid().get_version_num() == 7
                && self.through.uuid().get_variant() == uuid::Variant::RFC4122,
            "invalid history barrier identity"
        );
        Ok(())
    }
    pub fn covers(&self, id: OperationId) -> bool {
        id <= self.through
    }
}

/// Read old context files unchanged; upgrading replaces the context itself, so
/// deleting a separate floor file cannot silently restore legacy admission.
/// Older readers reject the upgraded context rather than forgetting the fence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Binding {
    Legacy(Context),
    Retired(Barrier),
}
impl Binding {
    pub fn validate(self, expected: &Context, domain: Domain) -> Result<Option<Barrier>> {
        match self {
            Self::Legacy(context) => {
                ensure!(&context == expected, "retained context mismatch");
                Ok(None)
            }
            Self::Retired(barrier) => {
                barrier.validate(expected, domain)?;
                Ok(Some(barrier))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::AllocationId;
    #[test]
    fn legacy_binding_and_barrier_are_unambiguous_and_downgrade_fails_closed() {
        let context = Context {
            allocation_id: AllocationId::generate(),
            generation: 1,
            boot_id: "boot".into(),
        };
        let old = serde_json::to_vec(&context).unwrap();
        assert!(
            serde_json::from_slice::<Binding>(&old)
                .unwrap()
                .validate(&context, Domain::Files)
                .unwrap()
                .is_none()
        );
        let barrier = Barrier {
            version: 1,
            context: context.clone(),
            domain: Domain::Files,
            through: OperationId::generate(),
        };
        let bytes = serde_json::to_vec(&Binding::Retired(barrier.clone())).unwrap();
        assert!(serde_json::from_slice::<Context>(&bytes).is_err());
        assert_eq!(
            serde_json::from_slice::<Binding>(&bytes)
                .unwrap()
                .validate(&context, Domain::Files)
                .unwrap(),
            Some(barrier.clone())
        );
        assert!(barrier.validate(&context, Domain::Commands).is_err());
        let mut other = context.clone();
        other.boot_id = "other".into();
        assert!(barrier.validate(&other, Domain::Files).is_err());
        other = context.clone();
        other.generation += 1;
        assert!(barrier.validate(&other, Domain::Files).is_err());
        let mut bad = barrier.clone();
        bad.version = 2;
        assert!(bad.validate(&context, Domain::Files).is_err());
        let mut value = serde_json::to_value(&barrier).unwrap();
        value["unknown"] = true.into();
        assert!(serde_json::from_value::<Binding>(value).is_err());
    }
    #[test]
    fn prefix_is_inclusive_and_has_no_clock_dependency() {
        let context = Context {
            allocation_id: AllocationId::generate(),
            generation: 1,
            boot_id: "boot".into(),
        };
        let id = |n| {
            format!("op_019a9fad-3000-7000-8000-{n:012x}")
                .parse()
                .unwrap()
        };
        let barrier = Barrier {
            version: 1,
            context,
            domain: Domain::Commands,
            through: id(10),
        };
        assert!(barrier.covers(id(9)));
        assert!(barrier.covers(id(10)));
        assert!(!barrier.covers(id(11)));
    }
}

impl TryFrom<crate::guest::HistoryBarrier> for Barrier {
    type Error = anyhow::Error;
    fn try_from(value: crate::guest::HistoryBarrier) -> Result<Self> {
        let domain = match crate::guest::HistoryDomain::try_from(value.domain)? {
            crate::guest::HistoryDomain::Commands => Domain::Commands,
            crate::guest::HistoryDomain::Files => Domain::Files,
            _ => anyhow::bail!("unspecified history domain"),
        };
        let barrier = Self {
            version: value.version,
            context: value
                .context
                .ok_or_else(|| anyhow::anyhow!("missing history context"))?
                .try_into()?,
            domain,
            through: value.through.parse()?,
        };
        barrier.validate(&barrier.context, domain)?;
        Ok(barrier)
    }
}
impl From<&Barrier> for crate::guest::HistoryBarrier {
    fn from(value: &Barrier) -> Self {
        Self {
            version: value.version,
            context: Some((&value.context).into()),
            domain: match value.domain {
                Domain::Commands => crate::guest::HistoryDomain::Commands as i32,
                Domain::Files => crate::guest::HistoryDomain::Files as i32,
            },
            through: value.through.to_string(),
        }
    }
}

#[cfg(test)]
mod wire_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn wire_barriers_reject_missing_scope_unknown_domain_and_invalid_ids() {
        let barrier = Barrier {
            version: 1,
            context: Context {
                allocation_id: crate::AllocationId::generate(),
                generation: 1,
                boot_id: "boot".into(),
            },
            domain: Domain::Commands,
            through: OperationId::generate(),
        };
        let wire: crate::guest::HistoryBarrier = (&barrier).into();
        assert_eq!(Barrier::try_from(wire.clone()).unwrap(), barrier);
        for mode in 0..6 {
            let mut bad = wire.clone();
            match mode {
                0 => bad.version = 0,
                1 => bad.context = None,
                2 => bad.domain = 0,
                3 => bad.domain = 99,
                4 => bad.through = "op_00000000-0000-4000-8000-000000000001".into(),
                _ => bad.context.as_mut().unwrap().boot_id.clear(),
            }
            assert!(Barrier::try_from(bad).is_err());
        }
    }
}

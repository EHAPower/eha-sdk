// Copyright The eha-sdk Contributors

//! 绑定固定身份的写入者声明。本模块不持有设备，也不提交 Stop。

use std::collections::{BTreeMap, BTreeSet};

const MAX_COMPONENT_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MemberKey {
    pub(crate) instance_id: String,
    pub(crate) target_id: String,
    pub(crate) uid: String,
    pub(crate) run_nonce: String,
    pub(crate) connection_generation: String,
}

impl MemberKey {
    pub(crate) fn new(
        instance_id: &str,
        target_id: &str,
        uid: &str,
        run_nonce: &str,
        connection_generation: &str,
    ) -> Result<Self, Error> {
        let member = Self {
            instance_id: instance_id.into(),
            target_id: target_id.into(),
            uid: uid.into(),
            run_nonce: run_nonce.into(),
            connection_generation: connection_generation.into(),
        };
        member.validate()?;
        Ok(member)
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if [
            &self.instance_id,
            &self.target_id,
            &self.uid,
            &self.run_nonce,
            &self.connection_generation,
        ]
        .into_iter()
        .all(|value| valid_component(value))
        {
            Ok(())
        } else {
            Err(Error::InvalidScope)
        }
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_COMPONENT_BYTES
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    Maintenance,
    Control,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Permissions {
    pub(crate) maintenance: bool,
    pub(crate) control: bool,
}

impl Permissions {
    fn permits(self, access: Access) -> bool {
        match access {
            Access::Maintenance => self.maintenance,
            Access::Control => self.control,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Grant {
    pub(crate) id: String,
    pub(crate) permissions: Permissions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidScope,
    WrongInstance,
    Capacity,
    PermissionDenied,
    Busy,
    NotFound,
    ScopeMismatch,
    ClaimantMismatch,
    SequenceExhausted,
    NotPending,
    OperationMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Reservation {
    pub(crate) id: String,
    pub(crate) claimant: String,
    pub(crate) members: Vec<MemberKey>,
    pub(crate) access: Access,
    pub(crate) grant_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Claim {
    pub(crate) reservation_id: String,
    pub(crate) claimant: String,
    pub(crate) member: MemberKey,
    pub(crate) pending: bool,
    pub(crate) pending_operation_id: Option<String>,
    pub(crate) unresolved: bool,
    pub(crate) released: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Release {
    pub(crate) released: Vec<MemberKey>,
    pub(crate) blocked: Vec<MemberKey>,
}

/// 由 owner 提供的效果证据，绝不从 HTTP 或本地尝试完成推断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectDisposition {
    NotSubmitted,
    KnownEffect,
    /// 维护操作没有持续控制效果；仅知本地提交结果。
    NotApplicable,
    Unresolved,
}

pub(crate) struct ReservationBook {
    instance_id: String,
    max_reservations: usize,
    max_members: usize,
    max_targets: usize,
    next_id: u64,
    next_grant_id: u64,
    grants: BTreeMap<MemberKey, Grant>,
    reservations: BTreeMap<String, Reservation>,
    claims: BTreeMap<String, Claim>,
}

impl ReservationBook {
    pub(crate) fn new(
        instance_id: &str,
        max_reservations: usize,
        max_members: usize,
        max_targets: usize,
    ) -> Result<Self, Error> {
        if !valid_component(instance_id)
            || max_reservations == 0
            || max_members == 0
            || max_targets == 0
            || max_members > max_targets
        {
            return Err(Error::InvalidScope);
        }
        Ok(Self {
            instance_id: instance_id.into(),
            max_reservations,
            max_members,
            max_targets,
            next_id: 1,
            next_grant_id: 1,
            grants: BTreeMap::new(),
            reservations: BTreeMap::new(),
            claims: BTreeMap::new(),
        })
    }

    fn validate_member(&self, member: &MemberKey) -> Result<(), Error> {
        member.validate()?;
        if member.instance_id != self.instance_id {
            return Err(Error::WrongInstance);
        }
        Ok(())
    }

    /// 仅在明确授权后端目标之后调用；不作为面向模型的授权 API。
    pub(crate) fn set_permissions(
        &mut self,
        member: MemberKey,
        permissions: Permissions,
    ) -> Result<Option<Grant>, Error> {
        self.validate_member(&member)?;
        if self
            .grants
            .get(&member)
            .is_some_and(|grant| grant.permissions == permissions)
        {
            return Ok(self.grants.get(&member).cloned());
        }
        if permissions != Permissions::default()
            && !self.grants.contains_key(&member)
            && self.grants.len() >= self.max_targets
        {
            return Err(Error::Capacity);
        }
        let next = if permissions == Permissions::default() {
            None
        } else {
            Some(
                self.next_grant_id
                    .checked_add(1)
                    .ok_or(Error::SequenceExhausted)?,
            )
        };
        // 退役完整的固定预留；排队或执行中的声明仍绑定原操作，
        // 因此撤销既不转移写入者，也不发送 Stop。
        let retired: Vec<_> = self
            .reservations
            .values()
            .filter(|reservation| reservation.members.contains(&member))
            .map(|reservation| (reservation.id.clone(), reservation.claimant.clone()))
            .collect();
        for (id, claimant) in retired {
            self.release(&id, &claimant)?;
        }
        if let Some(next) = next {
            let grant = Grant {
                id: format!("{}-grant-{}", self.instance_id, self.next_grant_id),
                permissions,
            };
            self.next_grant_id = next;
            self.grants.insert(member, grant.clone());
            Ok(Some(grant))
        } else {
            self.grants.remove(&member);
            Ok(None)
        }
    }

    pub(crate) fn grant(&self, member: &MemberKey) -> Option<&Grant> {
        self.grants.get(member)
    }

    pub(crate) fn reservation(&self, id: &str) -> Option<&Reservation> {
        self.reservations.get(id)
    }

    pub(crate) fn grant_members(&self) -> Vec<MemberKey> {
        self.grants.keys().cloned().collect()
    }

    fn require_permission(&self, member: &MemberKey, access: Access) -> Result<(), Error> {
        self.validate_member(member)?;
        if self
            .grants
            .get(member)
            .is_some_and(|grant| grant.permissions.permits(access))
        {
            Ok(())
        } else {
            Err(Error::PermissionDenied)
        }
    }

    /// 调用者必须在同一 owner 步骤中验证全部当前 owner 事实，不得等待。
    pub(crate) fn reserve(
        &mut self,
        claimant: &str,
        members: Vec<MemberKey>,
        access: Access,
    ) -> Result<Reservation, Error> {
        if !valid_component(claimant) || members.is_empty() || members.len() > self.max_members {
            return Err(Error::InvalidScope);
        }
        let mut uids = BTreeSet::new();
        for member in &members {
            self.validate_member(member)?;
            if !uids.insert(&member.uid) {
                return Err(Error::InvalidScope);
            }
        }
        for member in &members {
            self.require_permission(member, access)?;
            if self.claims.contains_key(&member.uid) {
                return Err(Error::Busy);
            }
        }
        if self.reservations.len() >= self.max_reservations
            || members.len() > self.max_targets - self.claims.len()
        {
            return Err(Error::Capacity);
        }
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let reservation = Reservation {
            id: format!("{}-reservation-{}", self.instance_id, self.next_id),
            claimant: claimant.into(),
            grant_ids: members
                .iter()
                .map(|member| self.grants[member].id.clone())
                .collect(),
            members,
            access,
        };
        for member in &reservation.members {
            self.claims.insert(
                member.uid.clone(),
                Claim {
                    reservation_id: reservation.id.clone(),
                    claimant: reservation.claimant.clone(),
                    member: member.clone(),
                    pending: false,
                    pending_operation_id: None,
                    unresolved: false,
                    released: false,
                },
            );
        }
        self.next_id = next_id;
        self.reservations
            .insert(reservation.id.clone(), reservation.clone());
        Ok(reservation)
    }

    pub(crate) fn authorize(
        &self,
        id: &str,
        claimant: &str,
        members: &[MemberKey],
        access: Access,
    ) -> Result<(), Error> {
        let reservation = self.reservations.get(id).ok_or(Error::NotFound)?;
        if reservation.claimant != claimant {
            return Err(Error::ClaimantMismatch);
        }
        if reservation.members != members {
            return Err(Error::ScopeMismatch);
        }
        if reservation.access != access {
            return Err(Error::PermissionDenied);
        }
        for (member, grant_id) in members.iter().zip(&reservation.grant_ids) {
            self.require_permission(member, access)?;
            if self.grants[member].id != *grant_id {
                return Err(Error::PermissionDenied);
            }
            let claim = self.claims.get(&member.uid).ok_or(Error::NotFound)?;
            if claim.pending || claim.unresolved || claim.released {
                return Err(Error::Busy);
            }
        }
        Ok(())
    }

    pub(crate) fn authorize_member(
        &self,
        id: &str,
        claimant: &str,
        member: &MemberKey,
        access: Access,
    ) -> Result<(), Error> {
        let reservation = self.reservations.get(id).ok_or(Error::NotFound)?;
        if reservation.claimant != claimant {
            return Err(Error::ClaimantMismatch);
        }
        if reservation.access != access {
            return Err(Error::PermissionDenied);
        }
        let index = reservation
            .members
            .iter()
            .position(|original| original == member)
            .ok_or(Error::ScopeMismatch)?;
        self.require_permission(member, access)?;
        if self.grants[member].id != reservation.grant_ids[index] {
            return Err(Error::PermissionDenied);
        }
        let claim = self.claims.get(&member.uid).ok_or(Error::NotFound)?;
        if claim.reservation_id != id || claim.member != *member {
            return Err(Error::ScopeMismatch);
        }
        if claim.pending || claim.unresolved || claim.released {
            return Err(Error::Busy);
        }
        Ok(())
    }

    pub(crate) fn begin_member_pending(
        &mut self,
        id: &str,
        operation_id: &str,
        claimant: &str,
        member: &MemberKey,
        access: Access,
    ) -> Result<(), Error> {
        if !valid_component(operation_id) {
            return Err(Error::InvalidScope);
        }
        self.authorize_member(id, claimant, member, access)?;
        let claim = self
            .claims
            .get_mut(&member.uid)
            .expect("authorized fixed member");
        claim.pending = true;
        claim.pending_operation_id = Some(operation_id.into());
        Ok(())
    }

    pub(crate) fn authorize_stop(&self, requested: &MemberKey) -> Result<(), Error> {
        self.require_permission(requested, Access::Control)
    }

    fn matching_claim(&mut self, id: &str, member: &MemberKey) -> Result<&mut Claim, Error> {
        self.validate_member(member)?;
        let claim = self.claims.get_mut(&member.uid).ok_or(Error::NotFound)?;
        if claim.reservation_id != id || claim.member != *member {
            return Err(Error::ScopeMismatch);
        }
        Ok(claim)
    }

    /// 工作入队前先固定声明，使释放不能在结果未决时转移声明。
    pub(crate) fn begin_pending(
        &mut self,
        id: &str,
        operation_id: &str,
        claimant: &str,
        members: &[MemberKey],
        access: Access,
    ) -> Result<(), Error> {
        if !valid_component(operation_id) {
            return Err(Error::InvalidScope);
        }
        self.authorize(id, claimant, members, access)?;
        for member in members {
            let claim = self.claims.get_mut(&member.uid).expect("authorized member");
            claim.pending = true;
            claim.pending_operation_id = Some(operation_id.into());
        }
        Ok(())
    }

    pub(crate) fn finish_pending(
        &mut self,
        id: &str,
        operation_id: &str,
        member: &MemberKey,
        effect: EffectDisposition,
    ) -> Result<(), Error> {
        let claim = self.matching_claim(id, member)?;
        if !claim.pending {
            return Err(Error::NotPending);
        }
        if claim.pending_operation_id.as_deref() != Some(operation_id) {
            return Err(Error::OperationMismatch);
        }
        claim.pending = false;
        claim.pending_operation_id = None;
        claim.unresolved |= effect == EffectDisposition::Unresolved;
        // 明确从未提交的完成结果可退役已释放的声明。实际已提交或效果未知的声明，
        // 绝不因本地等待者结束而转移。
        let retire =
            claim.released && !claim.unresolved && effect == EffectDisposition::NotSubmitted;
        if retire {
            self.claims.remove(&member.uid);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn mark_unresolved(&mut self, id: &str, member: &MemberKey) -> Result<(), Error> {
        self.matching_claim(id, member)?.unresolved = true;
        Ok(())
    }

    pub(crate) fn release(&mut self, id: &str, claimant: &str) -> Result<Release, Error> {
        let reservation = self.reservations.get(id).ok_or(Error::NotFound)?;
        if reservation.claimant != claimant {
            return Err(Error::ClaimantMismatch);
        }
        let reservation = self.reservations.remove(id).expect("checked reservation");
        let mut result = Release {
            released: Vec::new(),
            blocked: Vec::new(),
        };
        for member in reservation.members {
            let claim = self
                .claims
                .get_mut(&member.uid)
                .expect("reserved member claim");
            if claim.pending || claim.unresolved {
                claim.released = true;
                result.blocked.push(member);
            } else {
                self.claims.remove(&member.uid);
                result.released.push(member);
            }
        }
        Ok(result)
    }

    pub(crate) fn claim(&self, uid: &str) -> Option<&Claim> {
        self.claims.get(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(target: &str, uid: &str) -> MemberKey {
        MemberKey::new("instance", target, uid, "nonce", "1").expect("test invariant")
    }

    fn book() -> ReservationBook {
        ReservationBook::new("instance", 2, 2, 3).expect("test invariant")
    }

    fn grant(book: &mut ReservationBook, member: &MemberKey) {
        book.set_permissions(
            member.clone(),
            Permissions {
                maintenance: true,
                control: true,
            },
        )
        .expect("test invariant");
    }

    #[test]
    fn default_readonly_and_changed_identity_are_not_authorized() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        assert_eq!(
            book.reserve("trusted-agent", vec![a.clone()], Access::Control),
            Err(Error::PermissionDenied)
        );
        grant(&mut book, &a);
        for changed in [
            MemberKey::new("instance", "usb:a", "uid-a", "new-nonce", "1").expect("test invariant"),
            MemberKey::new("instance", "usb:a", "uid-a", "nonce", "2").expect("test invariant"),
        ] {
            assert_eq!(book.authorize_stop(&changed), Err(Error::PermissionDenied));
        }
        let foreign =
            MemberKey::new("other", "usb:a", "uid-a", "nonce", "1").expect("test invariant");
        assert_eq!(
            book.set_permissions(foreign, Permissions::default()),
            Err(Error::WrongInstance)
        );
    }

    #[test]
    fn group_preflight_is_atomic_and_uid_aliases_conflict() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        let b = member("can:1", "uid-b");
        grant(&mut book, &a);
        assert_eq!(
            book.reserve("one", vec![a.clone(), b.clone()], Access::Control),
            Err(Error::PermissionDenied)
        );
        grant(&mut book, &b);
        let held = book
            .reserve("one", vec![b.clone()], Access::Control)
            .expect("test invariant");
        assert_eq!(
            book.reserve("two", vec![a.clone(), b.clone()], Access::Control),
            Err(Error::Busy)
        );
        assert!(
            book.reserve("two", vec![a.clone()], Access::Control)
                .is_ok()
        );
        assert_eq!(
            book.authorize(&held.id, "one", &[b], Access::Control),
            Ok(())
        );
        let alias = member("can:2", "uid-a");
        grant(&mut book, &alias);
        assert_eq!(
            book.reserve("three", vec![alias], Access::Control),
            Err(Error::Busy)
        );
    }

    #[test]
    fn duplicate_uid_is_not_a_group_and_membership_is_fixed() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        let alias = member("can:1", "uid-a");
        grant(&mut book, &a);
        assert_eq!(
            book.reserve("one", vec![a.clone(), alias], Access::Control),
            Err(Error::InvalidScope)
        );
        let b = member("usb:b", "uid-b");
        grant(&mut book, &b);
        let held = book
            .reserve("one", vec![a.clone(), b.clone()], Access::Control)
            .expect("test invariant");
        assert_eq!(
            book.authorize(&held.id, "one", std::slice::from_ref(&a), Access::Control),
            Err(Error::ScopeMismatch)
        );
        assert_eq!(
            book.authorize(&held.id, "two", &[a.clone(), b.clone()], Access::Control),
            Err(Error::ClaimantMismatch)
        );
        assert_eq!(
            book.authorize(&held.id, "one", &[a, b], Access::Maintenance),
            Err(Error::PermissionDenied)
        );
    }

    #[test]
    fn manual_stop_bypasses_other_writer_but_not_target_permission() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        grant(&mut book, &a);
        let held = book
            .reserve("other-agent", vec![a.clone()], Access::Control)
            .expect("test invariant");
        assert_eq!(book.authorize_stop(&a), Ok(()));
        book.set_permissions(
            a.clone(),
            Permissions {
                maintenance: true,
                control: false,
            },
        )
        .expect("test invariant");
        assert_eq!(book.authorize_stop(&a), Err(Error::PermissionDenied));
        assert_eq!(
            book.authorize(&held.id, "other-agent", &[a], Access::Control),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn release_preserves_unknown_claim_and_all_its_original_facts() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        let b = member("usb:b", "uid-b");
        grant(&mut book, &a);
        grant(&mut book, &b);
        let held = book
            .reserve("one", vec![a.clone(), b.clone()], Access::Control)
            .expect("test invariant");
        book.mark_unresolved(&held.id, &a).expect("test invariant");
        let released = book.release(&held.id, "one").expect("test invariant");
        assert_eq!(released.released, vec![b.clone()]);
        assert_eq!(released.blocked, vec![a.clone()]);
        let claim = book.claim(&a.uid).expect("test invariant");
        assert_eq!(claim.member, a);
        assert_eq!(claim.reservation_id, held.id);
        assert_eq!(claim.claimant, "one");
        assert!(claim.unresolved);
        assert!(claim.released);
        assert_eq!(
            book.reserve("two", vec![a.clone()], Access::Control),
            Err(Error::Busy)
        );
        let replacement =
            MemberKey::new("instance", "usb:new", "uid-a", "new", "2").expect("test invariant");
        grant(&mut book, &replacement);
        assert_eq!(
            book.reserve("two", vec![replacement], Access::Control),
            Err(Error::Busy)
        );
        assert!(book.reserve("two", vec![b], Access::Control).is_ok());
    }

    #[test]
    fn pending_work_pins_release_and_late_completion_does_not_transfer() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        grant(&mut book, &a);
        let held = book
            .reserve("one", vec![a.clone()], Access::Control)
            .expect("test invariant");
        book.begin_pending(
            &held.id,
            "op-a",
            "one",
            std::slice::from_ref(&a),
            Access::Control,
        )
        .expect("test invariant");
        assert_eq!(
            book.authorize(&held.id, "one", std::slice::from_ref(&a), Access::Control),
            Err(Error::Busy)
        );
        assert_eq!(
            book.release(&held.id, "one")
                .expect("test invariant")
                .blocked,
            vec![a.clone()]
        );
        book.finish_pending(&held.id, "op-a", &a, EffectDisposition::KnownEffect)
            .expect("test invariant");
        assert!(book.claim(&a.uid).expect("test invariant").released);
        assert_eq!(
            book.reserve("two", vec![a], Access::Control),
            Err(Error::Busy)
        );
    }

    #[test]
    fn known_pending_completion_does_not_clear_prior_unknown() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        grant(&mut book, &a);
        let held = book
            .reserve("one", vec![a.clone()], Access::Control)
            .expect("test invariant");
        book.begin_pending(
            &held.id,
            "op-a",
            "one",
            std::slice::from_ref(&a),
            Access::Control,
        )
        .expect("test invariant");
        book.mark_unresolved(&held.id, &a).expect("test invariant");
        book.finish_pending(&held.id, "op-a", &a, EffectDisposition::KnownEffect)
            .expect("test invariant");
        assert_eq!(
            book.authorize(&held.id, "one", std::slice::from_ref(&a), Access::Control),
            Err(Error::Busy)
        );
        assert_eq!(
            book.release(&held.id, "one")
                .expect("test invariant")
                .blocked,
            vec![a]
        );
    }

    #[test]
    fn capacity_and_counter_refuse_before_claiming_any_member() {
        let mut book = ReservationBook::new("instance", 1, 2, 2).expect("test invariant");
        let a = member("usb:a", "uid-a");
        let b = member("usb:b", "uid-b");
        grant(&mut book, &a);
        grant(&mut book, &b);
        let held = book
            .reserve("one", vec![a.clone()], Access::Control)
            .expect("test invariant");
        assert_eq!(
            book.reserve("two", vec![b.clone()], Access::Control),
            Err(Error::Capacity)
        );
        book.release(&held.id, "one").expect("test invariant");
        book.next_id = u64::MAX;
        assert_eq!(
            book.reserve("two", vec![b.clone()], Access::Control),
            Err(Error::SequenceExhausted)
        );
        assert!(book.claim(&b.uid).is_none());
        assert_eq!(
            book.reserve("two", vec![a], Access::Control),
            Err(Error::SequenceExhausted)
        );
    }

    #[test]
    fn group_pending_preflight_never_pins_only_the_first_member() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        let b = member("usb:b", "uid-b");
        grant(&mut book, &a);
        grant(&mut book, &b);
        let members = vec![a.clone(), b.clone()];
        let held = book
            .reserve("one", members.clone(), Access::Control)
            .expect("test invariant");
        book.mark_unresolved(&held.id, &b).expect("test invariant");
        assert_eq!(
            book.begin_pending(&held.id, "op-a", "one", &members, Access::Control),
            Err(Error::Busy)
        );
        assert!(!book.claim(&a.uid).expect("test invariant").pending);
        assert!(!book.claim(&b.uid).expect("test invariant").pending);
    }

    #[test]
    fn delayed_old_cleanup_and_completion_do_not_affect_new_claim() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        grant(&mut book, &a);
        let old = book
            .reserve("one", vec![a.clone()], Access::Control)
            .expect("test invariant");
        book.begin_pending(
            &old.id,
            "op-a",
            "one",
            std::slice::from_ref(&a),
            Access::Control,
        )
        .expect("test invariant");
        book.finish_pending(&old.id, "op-a", &a, EffectDisposition::NotSubmitted)
            .expect("test invariant");
        book.release(&old.id, "one").expect("test invariant");
        let new = book
            .reserve("two", vec![a.clone()], Access::Control)
            .expect("test invariant");
        assert_ne!(old.id, new.id);
        assert_eq!(book.release(&old.id, "one"), Err(Error::NotFound));
        assert_eq!(book.mark_unresolved(&old.id, &a), Err(Error::ScopeMismatch));
        assert_eq!(
            book.finish_pending(&old.id, "op-a", &a, EffectDisposition::Unresolved),
            Err(Error::ScopeMismatch)
        );
        assert_eq!(
            book.authorize(&new.id, "two", &[a], Access::Control),
            Ok(())
        );
    }

    #[test]
    fn duplicate_previous_operation_completion_never_finishes_new_pending_work() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        grant(&mut book, &a);
        let held = book
            .reserve("one", vec![a.clone()], Access::Control)
            .expect("test invariant");
        book.begin_pending(
            &held.id,
            "op-a",
            "one",
            std::slice::from_ref(&a),
            Access::Control,
        )
        .expect("test invariant");
        book.finish_pending(&held.id, "op-a", &a, EffectDisposition::KnownEffect)
            .expect("test invariant");
        book.begin_pending(
            &held.id,
            "op-b",
            "one",
            std::slice::from_ref(&a),
            Access::Control,
        )
        .expect("test invariant");
        assert_eq!(
            book.finish_pending(&held.id, "op-a", &a, EffectDisposition::KnownEffect),
            Err(Error::OperationMismatch)
        );
        assert_eq!(
            book.claim(&a.uid)
                .expect("test invariant")
                .pending_operation_id
                .as_deref(),
            Some("op-b")
        );
        assert_eq!(
            book.release(&held.id, "one")
                .expect("test invariant")
                .blocked,
            vec![a.clone()]
        );
        assert_eq!(
            book.reserve("two", vec![a], Access::Control),
            Err(Error::Busy)
        );
    }
    #[test]
    fn revoke_and_reenable_never_reactivates_old_reservation_or_pending_attempt() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        grant(&mut book, &a);
        let old_grant = book.grant(&a).expect("test invariant").id.clone();
        let held = book
            .reserve("agent", vec![a.clone()], Access::Maintenance)
            .expect("test invariant");
        book.begin_pending(
            &held.id,
            "op-a",
            "agent",
            std::slice::from_ref(&a),
            Access::Maintenance,
        )
        .expect("test invariant");
        book.set_permissions(a.clone(), Permissions::default())
            .expect("test invariant");
        grant(&mut book, &a);
        assert_ne!(book.grant(&a).expect("test invariant").id, old_grant);
        assert_eq!(
            book.authorize(
                &held.id,
                "agent",
                std::slice::from_ref(&a),
                Access::Maintenance
            ),
            Err(Error::NotFound)
        );
        assert_eq!(
            book.reserve("agent", vec![a.clone()], Access::Maintenance),
            Err(Error::Busy)
        );
        book.finish_pending(&held.id, "op-a", &a, EffectDisposition::Unresolved)
            .expect("test invariant");
        assert!(book.claims[&a.uid].unresolved);
        assert!(book.claims[&a.uid].released);
    }
    #[test]
    fn group_children_keep_original_membership_and_independent_attempt_ownership() {
        let mut book = book();
        let a = member("usb:a", "uid-a");
        let b = member("can:b", "uid-b");
        grant(&mut book, &a);
        grant(&mut book, &b);
        let held = book
            .reserve("agent", vec![a.clone(), b.clone()], Access::Control)
            .expect("test invariant");
        book.begin_member_pending(&held.id, "op-a", "agent", &a, Access::Control)
            .expect("test invariant");
        book.begin_member_pending(&held.id, "op-b", "agent", &b, Access::Control)
            .expect("test invariant");
        let alias = member("can:a", "uid-a");
        grant(&mut book, &alias);
        assert_eq!(
            book.authorize_member(&held.id, "agent", &alias, Access::Control),
            Err(Error::ScopeMismatch)
        );
        assert_eq!(
            book.finish_pending(&held.id, "op-a", &b, EffectDisposition::KnownEffect),
            Err(Error::OperationMismatch)
        );
        book.finish_pending(&held.id, "op-a", &a, EffectDisposition::KnownEffect)
            .expect("test invariant");
        assert!(book.claims[&b.uid].pending);
        assert_eq!(
            book.reservation(&held.id).expect("test invariant").members,
            vec![a, b]
        );
    }
}

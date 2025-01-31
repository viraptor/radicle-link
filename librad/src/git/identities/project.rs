// Copyright © 2019-2020 The Radicle Foundation <hello@radicle.foundation>
//
// This file is part of radicle-link, distributed under the GPLv3 with Radicle
// Linking Exception. For full terms see the included LICENSE file.

use std::{convert::TryFrom, fmt::Debug};

use either::Either;
use git_ext::{is_not_found_err, OneLevel};
use nonempty::NonEmpty;

use super::{
    super::{
        refs::Refs as Sigrefs,
        storage::{self, Storage},
        types::{namespace, reference, Force, Reference, Single, SymbolicRef},
    },
    common,
    error::Error,
    local::LocalIdentity,
};
use crate::{
    identities::{
        self,
        git::{Identities, IndirectDelegation, Project, Revision, VerifiedProject, Verifying},
        urn,
    },
    peer::PeerId,
};

pub use identities::{git::Urn, payload::ProjectPayload};

type Namespace = namespace::Namespace<Revision>;

/// Read a [`Project`] from the tip of the ref [`Urn::path`] points to.
///
/// If the ref is not found, `None` is returned.
#[tracing::instrument(level = "trace", skip(storage), err)]
pub fn get(storage: &Storage, urn: &Urn) -> Result<Option<Project>, Error> {
    match storage.reference(&Reference::try_from(urn)?) {
        Ok(Some(reference)) => {
            let tip = reference.peel_to_commit()?.id();
            Ok(Some(identities(storage).get(tip)?))
        },

        Ok(None) => Ok(None),
        Err(storage::Error::Git(e)) if is_not_found_err(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Read and verify the [`Project`] pointed to by `urn`.
///
/// If the ref pointed to by [`Urn::path`] is not found, `None` is returned.
///
/// # Caveats
///
/// Keep in mind that the `content_id` of a successfully verified project may
/// not be the same as the tip of the ref [`Urn::path`] points to. That is, this
/// function cannot be used to assert that the state after an [`update`] is
/// valid.
#[tracing::instrument(level = "debug", skip(storage), err)]
pub fn verify(storage: &Storage, urn: &Urn) -> Result<Option<VerifiedProject>, Error> {
    match storage.reference(&Reference::try_from(urn)?) {
        Ok(Some(reference)) => {
            let tip = reference.peel_to_commit()?.id();
            let lookup = |urn| {
                let refname = Reference::rad_id(Namespace::from(urn)).to_string();
                storage.as_raw().refname_to_id(&refname)
            };
            identities(storage)
                .verify(tip, lookup)
                .map(Some)
                .map_err(|e| Error::Verify(e.into()))
        },

        Ok(None) => Ok(None),
        Err(storage::Error::Git(e)) if is_not_found_err(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Create a new [`Project`].
#[tracing::instrument(level = "debug", skip(storage, whoami), err)]
pub fn create<P>(
    storage: &Storage,
    whoami: LocalIdentity,
    payload: P,
    delegations: IndirectDelegation,
) -> Result<Project, Error>
where
    P: Into<ProjectPayload> + Debug,
{
    let project = identities(storage).create(payload.into(), delegations, storage.signer())?;
    let urn = project.urn();
    ProjectRefs::Create(&project).apply(storage)?;
    whoami.link(storage, &urn)?;
    Sigrefs::update(storage, &urn)?;

    Ok(project)
}

/// Update the [`Project`] at `urn`.
#[tracing::instrument(level = "debug", skip(storage), err)]
pub fn update<L, P, D>(
    storage: &Storage,
    urn: &Urn,
    whoami: L,
    payload: P,
    delegations: D,
) -> Result<Project, Error>
where
    L: Into<Option<LocalIdentity>> + Debug,
    P: Into<Option<ProjectPayload>> + Debug,
    D: Into<Option<IndirectDelegation>> + Debug,
{
    let prev = get(storage, urn)?.ok_or_else(|| Error::NotFound(urn.clone()))?;
    let prev = Verifying::from(prev).signed()?;
    let next = identities(storage).update(prev, payload, delegations, storage.signer())?;

    ProjectRefs::Update(&next, "update").apply(storage)?;
    if let Some(local_id) = whoami.into() {
        local_id.link(storage, urn)?;
    }
    Sigrefs::update(storage, urn)?;

    Ok(next)
}

/// Merge and sign the [`Project`] state as seen by `from`.
#[tracing::instrument(level = "debug", skip(storage), err)]
pub fn merge(storage: &Storage, urn: &Urn, from: PeerId) -> Result<Project, Error> {
    let ours = get(storage, urn)?.ok_or_else(|| Error::NotFound(urn.clone()))?;
    let theirs = {
        let (path, rad) = OneLevel::from_qualified(urn::DEFAULT_PATH.clone());
        let rad = rad.expect("default path should be refs/rad/id");
        let their_urn = Urn {
            id: urn.id,
            path: Some(reflike!("refs/remotes").join(from).join(rad).join(path)),
        };
        get(storage, &their_urn)?.ok_or(Error::NotFound(their_urn))?
    };

    let ours = Verifying::from(ours).signed()?;
    let theirs = Verifying::from(theirs).signed()?;
    let next = identities(storage).update_from(ours, theirs, storage.signer())?;

    ProjectRefs::Update(&next, &format!("merge from {}", from)).apply(storage)?;
    Sigrefs::update(storage, urn)?;

    Ok(next)
}

/// Returns `true` if the `left` or the `right` projects do not share the same
/// commit history.
///
/// # Errors
///
///   * If the `left` project could not be found
///   * If the `right` project could not be found
pub fn is_fork(storage: &Storage, left: &Urn, right: &Urn) -> Result<bool, Error> {
    let left = verify(storage, left)?.ok_or_else(|| Error::NotFound(left.clone()))?;
    let right = verify(storage, right)?.ok_or_else(|| Error::NotFound(right.clone()))?;
    let verified = verified(&storage);
    Ok(verified.is_fork(&left, &right)?)
}

/// Given a list projects -- assumed to be the same project -- return the latest
/// revision tip.
pub fn latest_tip(storage: &Storage, projects: NonEmpty<Project>) -> Result<git2::Oid, Error> {
    // FIXME: Should we ensure that all the projects have the same URN?
    Ok(identities(storage).latest_tip(projects)?)
}

enum ProjectRefs<'a> {
    Create(&'a Project),
    Update(&'a Project, &'a str),
}

impl<'a> ProjectRefs<'a> {
    pub fn apply(&self, storage: &Storage) -> Result<(), Error> {
        for symref in self.delegates() {
            symref.create(storage.as_raw())?;
        }
        match self {
            Self::Create(project) => {
                common::IdRef::from(&project.urn()).create(storage, project.content_id)
            },
            Self::Update(project, msg) => {
                common::IdRef::from(&project.urn()).update(storage, project.content_id, msg)
            },
        }?;

        Ok(())
    }

    fn project(&self) -> &Project {
        match self {
            Self::Create(project) => project,
            Self::Update(project, _) => project,
        }
    }

    fn delegates(
        &'a self,
    ) -> impl Iterator<
        Item = SymbolicRef<
            reference::Reference<Namespace, PeerId, Single>,
            reference::Reference<Namespace, PeerId, Single>,
        >,
    > + 'a {
        let source = self.project().urn();
        self.project()
            .delegations()
            .iter()
            .filter_map(Either::right)
            .map(move |id| {
                let urn = id.urn();
                SymbolicRef {
                    source: Reference::rad_delegate(Namespace::from(&source), &urn),
                    target: Reference::rad_id(Namespace::from(&urn)),
                    force: Force::True,
                }
            })
    }
}

fn identities(storage: &Storage) -> Identities<Project> {
    storage.identities()
}

fn verified(storage: &Storage) -> Identities<VerifiedProject> {
    storage.identities()
}

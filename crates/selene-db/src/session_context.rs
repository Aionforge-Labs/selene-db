//! Facade session state and immutable dependency inspection.

use std::{cell::RefCell, collections::BTreeMap, sync::Arc};

use selene_core::DbString;
use selene_profile::{current_profile_identity, current_session_defaults};

use crate::{
    AuthorizationId, CatalogGeneration, Error, GeneralParameter, GraphDescriptor, GraphId,
    Principal, RequestContext, Result, SchemaDescriptor, SchemaId, Transaction, TransactionState,
    session::cache::{DependencyStamp, RequestPlanKey, SessionPlanCache},
    session::control::ResolvedSessionControl,
    transaction::DetachedTransaction,
};

/// Facade-owned copy of the generated profile identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProfileIdentity {
    profile_id: String,
    source_format_version: u32,
    generator_version: u32,
    canonical_hash: String,
}

impl ProfileIdentity {
    pub(crate) fn current() -> Self {
        let identity = current_profile_identity();
        Self {
            profile_id: identity.profile_id().to_owned(),
            source_format_version: identity.source_format_version(),
            generator_version: identity.generator_version(),
            canonical_hash: identity.canonical_hash().to_owned(),
        }
    }

    /// Return the stable target-profile identifier.
    #[must_use]
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    /// Return the incompatible profile source-format version.
    #[must_use]
    pub const fn source_format_version(&self) -> u32 {
        self.source_format_version
    }

    /// Return the deterministic generator-contract version.
    #[must_use]
    pub const fn generator_version(&self) -> u32 {
        self.generator_version
    }

    /// Return the canonical profile BLAKE3 hash.
    #[must_use]
    pub fn canonical_hash(&self) -> &str {
        &self.canonical_hash
    }
}

/// Fixed session displacement from UTC.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TimeZoneDisplacement {
    seconds: i32,
}

impl TimeZoneDisplacement {
    fn from_profile() -> Self {
        Self {
            seconds: current_session_defaults().time_zone().seconds(),
        }
    }

    pub(crate) const fn from_seconds(seconds: i32) -> Self {
        Self { seconds }
    }

    /// Return the signed UTC displacement in seconds.
    #[must_use]
    pub const fn seconds(self) -> i32 {
        self.seconds
    }
}

/// Snapshot inspection of session parameter state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionParameters {
    count: u64,
}

impl SessionParameters {
    fn from_count(count: usize) -> Self {
        Self {
            count: count as u64,
        }
    }

    /// Return the number of session parameters.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.count
    }

    /// Return whether the session parameter dictionary is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Session request-slot inspection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RequestSlotState {
    /// No request is active.
    Vacant,
    /// One request context is associated with the session.
    Active,
}

/// Session transaction-slot inspection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransactionSlotState {
    /// No transaction is active.
    Vacant,
    /// Requests may execute and stage work.
    Active,
    /// A statement failed and detached work was discarded.
    Failed,
    /// Commit validation/publication is in progress.
    Committing,
    /// Detached work was discarded.
    RolledBack,
    /// The complete successor state was published and acknowledged.
    Committed,
    /// Publication completed but acknowledgement was uncertain.
    Indeterminate,
}

impl From<TransactionState> for TransactionSlotState {
    fn from(state: TransactionState) -> Self {
        match state {
            TransactionState::Active => Self::Active,
            TransactionState::Failed => Self::Failed,
            TransactionState::Committing => Self::Committing,
            TransactionState::RolledBack => Self::RolledBack,
            TransactionState::Committed => Self::Committed,
            TransactionState::Indeterminate => Self::Indeterminate,
        }
    }
}

/// Session termination inspection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionTerminationState {
    /// The session accepts requests.
    Active,
    /// The session rejects every future request.
    Closed,
}

/// Snapshot of the session's current stable dependencies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDependencySummary {
    current_schema: SchemaId,
    current_graph: GraphId,
    home_schema: Option<SchemaId>,
    home_graph: Option<GraphId>,
    profile: ProfileIdentity,
}

impl SessionDependencySummary {
    /// Return the stable current schema identity.
    #[must_use]
    pub const fn current_schema(&self) -> SchemaId {
        self.current_schema
    }

    /// Return the stable current graph identity.
    #[must_use]
    pub const fn current_graph(&self) -> GraphId {
        self.current_graph
    }

    /// Return the stable home schema identity, when configured.
    #[must_use]
    pub const fn home_schema(&self) -> Option<SchemaId> {
        self.home_schema
    }

    /// Return the stable home graph identity, when configured.
    #[must_use]
    pub const fn home_graph(&self) -> Option<GraphId> {
        self.home_graph
    }

    /// Return the generated profile identity used by this session.
    #[must_use]
    pub const fn profile_identity(&self) -> &ProfileIdentity {
        &self.profile
    }
}

pub(crate) struct SessionContextParts {
    pub(crate) authorization_id: Option<AuthorizationId>,
    pub(crate) principal: Option<Principal>,
    pub(crate) home_schema: Option<SchemaDescriptor>,
    pub(crate) home_graph: Option<GraphDescriptor>,
    pub(crate) current_schema: SchemaDescriptor,
    pub(crate) current_graph: GraphDescriptor,
    pub(crate) catalog_generation: CatalogGeneration,
}

struct SessionState {
    current_schema: SchemaDescriptor,
    current_graph: GraphDescriptor,
    catalog_generation: CatalogGeneration,
    time_zone: jiff::tz::TimeZone,
    time_zone_displacement: TimeZoneDisplacement,
    parameters: BTreeMap<DbString, GeneralParameter>,
    active_request: Option<Arc<RequestContext>>,
    transaction: Option<DetachedTransaction>,
    termination: SessionTerminationState,
    characteristic_epoch: u64,
    plan_cache: SessionPlanCache,
}

pub(crate) struct ActiveRequestGuard<'a> {
    state: &'a RefCell<SessionState>,
    context: Arc<RequestContext>,
}

pub(crate) struct TransactionCheckout<'a> {
    state: &'a RefCell<SessionState>,
    transaction: Option<DetachedTransaction>,
}

impl TransactionCheckout<'_> {
    pub(crate) fn as_ref(&self) -> Option<&DetachedTransaction> {
        self.transaction.as_ref()
    }

    pub(crate) fn as_mut(&mut self) -> Option<&mut DetachedTransaction> {
        self.transaction.as_mut()
    }

    pub(crate) fn replace(
        &mut self,
        transaction: DetachedTransaction,
    ) -> Option<DetachedTransaction> {
        self.transaction.replace(transaction)
    }

    pub(crate) fn take(&mut self) -> Option<DetachedTransaction> {
        self.transaction.take()
    }
}

impl Drop for TransactionCheckout<'_> {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(transaction) = self.transaction.as_mut()
            && matches!(
                transaction.descriptor().state(),
                TransactionState::Active | TransactionState::Committing
            )
        {
            transaction.abandon_on_unwind();
        }
        self.state.borrow_mut().transaction = self.transaction.take();
    }
}

impl Drop for ActiveRequestGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        if std::thread::panicking()
            && let Some(transaction) = state.transaction.as_mut()
            && matches!(
                transaction.descriptor().state(),
                TransactionState::Active | TransactionState::Committing
            )
        {
            transaction.abandon_on_unwind();
        }
        if state
            .active_request
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &self.context))
        {
            state.active_request = None;
        }
    }
}

/// Immutable creation dependencies and controlled state for one session.
///
/// The context owns facade descriptors, typed parameters, and at most one
/// request context. It retains no catalog read snapshot, runtime graph
/// allocation, lifecycle lease, or lower engine handle.
pub struct SessionContext {
    authorization_id: Option<AuthorizationId>,
    principal: Option<Principal>,
    home_schema: Option<SchemaDescriptor>,
    home_graph: Option<GraphDescriptor>,
    default_schema: SchemaDescriptor,
    default_graph: GraphDescriptor,
    profile: ProfileIdentity,
    state: RefCell<SessionState>,
}

impl SessionContext {
    pub(crate) fn new(parts: SessionContextParts) -> Self {
        let profile = ProfileIdentity::current();
        let displacement = TimeZoneDisplacement::from_profile();
        let offset = jiff::tz::Offset::from_seconds(displacement.seconds())
            .expect("generated session time-zone displacement is valid");
        Self {
            authorization_id: parts.authorization_id,
            principal: parts.principal,
            home_schema: parts.home_schema,
            home_graph: parts.home_graph,
            default_schema: parts.current_schema.clone(),
            default_graph: parts.current_graph.clone(),
            profile,
            state: RefCell::new(SessionState {
                current_schema: parts.current_schema,
                current_graph: parts.current_graph,
                catalog_generation: parts.catalog_generation,
                time_zone: jiff::tz::TimeZone::fixed(offset),
                time_zone_displacement: displacement,
                parameters: BTreeMap::new(),
                active_request: None,
                transaction: None,
                termination: SessionTerminationState::Active,
                characteristic_epoch: 0,
                plan_cache: SessionPlanCache::default(),
            }),
        }
    }

    /// Return the authorization ID supplied at creation.
    #[must_use]
    pub const fn authorization_id(&self) -> Option<&AuthorizationId> {
        self.authorization_id.as_ref()
    }

    /// Return the resolved principal.
    #[must_use]
    pub const fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    /// Return the copied home schema descriptor.
    #[must_use]
    pub const fn home_schema(&self) -> Option<&SchemaDescriptor> {
        self.home_schema.as_ref()
    }

    /// Return the copied home graph descriptor.
    #[must_use]
    pub const fn home_graph(&self) -> Option<&GraphDescriptor> {
        self.home_graph.as_ref()
    }

    /// Return the copied current schema descriptor.
    #[must_use]
    pub fn current_schema(&self) -> SchemaDescriptor {
        self.state.borrow().current_schema.clone()
    }

    /// Return the copied current graph descriptor.
    #[must_use]
    pub fn current_graph(&self) -> GraphDescriptor {
        self.state.borrow().current_graph.clone()
    }

    /// Return the catalog generation observed by the latest applied control.
    #[must_use]
    pub fn catalog_generation(&self) -> CatalogGeneration {
        self.state.borrow().catalog_generation
    }

    /// Return a snapshot of the current catalog/profile dependency summary.
    #[must_use]
    pub fn dependencies(&self) -> SessionDependencySummary {
        let state = self.state.borrow();
        SessionDependencySummary {
            current_schema: state.current_schema.id,
            current_graph: state.current_graph.id,
            home_schema: self.home_schema.as_ref().map(|schema| schema.id),
            home_graph: self.home_graph.as_ref().map(|graph| graph.id),
            profile: self.profile.clone(),
        }
    }

    /// Return the copied generated profile identity.
    #[must_use]
    pub const fn profile_identity(&self) -> &ProfileIdentity {
        &self.profile
    }

    /// Return the current session time-zone displacement.
    #[must_use]
    pub fn time_zone(&self) -> TimeZoneDisplacement {
        self.state.borrow().time_zone_displacement
    }

    /// Return a count snapshot of the current session parameter dictionary.
    #[must_use]
    pub fn parameters(&self) -> SessionParameters {
        SessionParameters::from_count(self.state.borrow().parameters.len())
    }

    /// Return whether a request context is currently associated with the session.
    #[must_use]
    pub fn request_slot(&self) -> RequestSlotState {
        if self.state.borrow().active_request.is_some() {
            RequestSlotState::Active
        } else {
            RequestSlotState::Vacant
        }
    }

    /// Clone the actual active request context, when the slot is occupied.
    #[must_use]
    pub fn current_request(&self) -> Option<Arc<RequestContext>> {
        self.state.borrow().active_request.as_ref().map(Arc::clone)
    }

    /// Return the precise transaction slot state.
    #[must_use]
    pub fn transaction_slot(&self) -> TransactionSlotState {
        self.state
            .borrow()
            .transaction
            .as_ref()
            .map_or(TransactionSlotState::Vacant, |transaction| {
                transaction.descriptor().state().into()
            })
    }

    /// Clone the immutable transaction inspection descriptor, when present.
    #[must_use]
    pub fn transaction(&self) -> Option<Transaction> {
        self.state
            .borrow()
            .transaction
            .as_ref()
            .map(|transaction| transaction.descriptor().clone())
    }

    #[cfg(test)]
    pub(crate) fn transaction_retains_detached_state(&self) -> Option<bool> {
        self.state
            .borrow()
            .transaction
            .as_ref()
            .map(crate::transaction::DetachedTransaction::retains_detached_state)
    }

    /// Return the session termination state.
    #[must_use]
    pub fn termination(&self) -> SessionTerminationState {
        self.state.borrow().termination
    }

    pub(crate) fn parameter_snapshot(&self) -> BTreeMap<DbString, GeneralParameter> {
        self.state.borrow().parameters.clone()
    }

    pub(crate) fn time_zone_value(&self) -> jiff::tz::TimeZone {
        self.state.borrow().time_zone.clone()
    }

    pub(crate) fn characteristic_epoch(&self) -> u64 {
        self.state.borrow().characteristic_epoch
    }

    pub(crate) fn reset_schema_target(&self) -> SchemaDescriptor {
        self.home_schema
            .clone()
            .unwrap_or_else(|| self.default_schema.clone())
    }

    pub(crate) fn reset_graph_target(&self) -> GraphDescriptor {
        self.home_graph
            .clone()
            .unwrap_or_else(|| self.default_graph.clone())
    }

    pub(crate) fn has_parameter(&self, name: &DbString) -> bool {
        self.state.borrow().parameters.contains_key(name)
    }

    pub(crate) fn cached_plan(
        &self,
        key: &RequestPlanKey,
        stamp: &DependencyStamp,
        valid: impl FnOnce(&selene_gql::PreparedCatalogPlan) -> bool,
    ) -> Option<selene_gql::PreparedCatalogPlan> {
        self.state.borrow_mut().plan_cache.lookup(key, stamp, valid)
    }

    pub(crate) fn cache_plan(
        &self,
        key: RequestPlanKey,
        stamp: DependencyStamp,
        plan: selene_gql::PreparedCatalogPlan,
    ) {
        self.state.borrow_mut().plan_cache.insert(key, stamp, plan);
    }

    pub(crate) fn apply_session_control(
        &self,
        control: ResolvedSessionControl,
        catalog_generation: CatalogGeneration,
    ) {
        let mut state = self.state.borrow_mut();
        match control {
            ResolvedSessionControl::NoOp => return,
            ResolvedSessionControl::SetValue { name, parameter } => {
                state.parameters.insert(name, parameter);
            }
            ResolvedSessionControl::SetTimeZone {
                zone,
                displacement_seconds,
            } => {
                state.time_zone = zone;
                state.time_zone_displacement =
                    TimeZoneDisplacement::from_seconds(displacement_seconds);
            }
            ResolvedSessionControl::SetSchema(schema) => state.current_schema = schema,
            ResolvedSessionControl::SetGraph(graph) => state.current_graph = graph,
            ResolvedSessionControl::ResetAllCharacteristics { schema, graph } => {
                state.current_schema = schema;
                state.current_graph = graph;
                let displacement = TimeZoneDisplacement::from_profile();
                let offset = jiff::tz::Offset::from_seconds(displacement.seconds())
                    .expect("generated session time-zone displacement is valid");
                state.time_zone = jiff::tz::TimeZone::fixed(offset);
                state.time_zone_displacement = displacement;
                state.parameters.clear();
            }
            ResolvedSessionControl::ResetSchema(schema) => state.current_schema = schema,
            ResolvedSessionControl::ResetGraph(graph) => state.current_graph = graph,
            ResolvedSessionControl::ResetParameters => state.parameters.clear(),
            ResolvedSessionControl::ResetTimeZone => {
                let displacement = TimeZoneDisplacement::from_profile();
                let offset = jiff::tz::Offset::from_seconds(displacement.seconds())
                    .expect("generated session time-zone displacement is valid");
                state.time_zone = jiff::tz::TimeZone::fixed(offset);
                state.time_zone_displacement = displacement;
            }
            ResolvedSessionControl::ResetParameter(name) => {
                state.parameters.remove(&name);
            }
            ResolvedSessionControl::Close => {
                state.parameters.clear();
                state.plan_cache.clear();
                state.termination = SessionTerminationState::Closed;
            }
        }
        state.catalog_generation = catalog_generation;
        state.plan_cache.clear();
        state.characteristic_epoch = state.characteristic_epoch.saturating_add(1);
    }

    #[cfg(test)]
    pub(crate) fn plan_cache_stats(&self) -> (u64, u64) {
        self.state.borrow().plan_cache.stats()
    }

    pub(crate) fn set_parameter(
        &self,
        name: DbString,
        parameter: GeneralParameter,
    ) -> Option<GeneralParameter> {
        let mut state = self.state.borrow_mut();
        let previous = state.parameters.insert(name, parameter);
        state.plan_cache.clear();
        state.characteristic_epoch = state.characteristic_epoch.saturating_add(1);
        previous
    }

    pub(crate) fn remove_parameter(&self, name: &str) -> Option<GeneralParameter> {
        let mut state = self.state.borrow_mut();
        let removed = state.parameters.remove(name);
        if removed.is_some() {
            state.plan_cache.clear();
            state.characteristic_epoch = state.characteristic_epoch.saturating_add(1);
        }
        removed
    }

    pub(crate) fn checkout_transaction(&self) -> TransactionCheckout<'_> {
        let transaction = self.state.borrow_mut().transaction.take();
        TransactionCheckout {
            state: &self.state,
            transaction,
        }
    }

    pub(crate) fn activate_request(
        &self,
        context: Arc<RequestContext>,
    ) -> Result<ActiveRequestGuard<'_>> {
        let mut state = self.state.borrow_mut();
        if state.termination == SessionTerminationState::Closed {
            return Err(Error::session_closed());
        }
        if state.active_request.is_some() {
            return Err(Error::request_already_active());
        }
        state.active_request = Some(Arc::clone(&context));
        drop(state);
        Ok(ActiveRequestGuard {
            state: &self.state,
            context,
        })
    }
}

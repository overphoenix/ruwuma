use std::{
    borrow::Borrow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    hash::Hash,
};

use futures_util::{future, stream, Future, StreamExt};
use itertools::Itertools;
use js_int::{int, Int};
use ruma_common::{EventId, MilliSecondsSinceUnixEpoch, RoomVersionId};
use ruma_events::{
    room::member::{MembershipState, RoomMemberEventContent},
    StateEventType, TimelineEventType,
};
use serde_json::from_str as from_json_str;
use tracing::{debug, instrument, trace, warn};

mod error;
pub mod event_auth;
mod power_levels;
pub mod room_version;
mod state_event;
#[cfg(test)]
mod test_utils;

pub use error::{Error, Result};
pub use event_auth::{auth_check, auth_types_for_event};
use power_levels::PowerLevelsContentFields;
pub use room_version::RoomVersion;
pub use state_event::Event;

/// A mapping of event type and state_key to some value `T`, usually an `EventId`.
pub type StateMap<T> = HashMap<(StateEventType, String), T>;

/// Resolve sets of state events as they come in.
///
/// Internally `StateResolution` builds a graph and an auth chain to allow for state conflict
/// resolution.
///
/// ## Arguments
///
/// * `state_sets` - The incoming state to resolve. Each `StateMap` represents a possible fork in
///   the state of a room.
///
/// * `auth_chain_sets` - The full recursive set of `auth_events` for each event in the
///   `state_sets`.
///
/// * `event_fetch` - Any event not found in the `event_map` will defer to this closure to find the
///   event.
///
/// ## Invariants
///
/// The caller of `resolve` must ensure that all the events are from the same room. Although this
/// function takes a `RoomId` it does not check that each event is part of the same room.
//#[instrument(level = "debug", skip(state_sets, auth_chain_sets, event_fetch))]
pub async fn resolve<'a, E, SetIter, Fetch, FetchFut, Exists, ExistsFut>(
    room_version: &RoomVersionId,
    state_sets: impl IntoIterator<IntoIter = SetIter> + Send,
    auth_chain_sets: &'a Vec<HashSet<E::Id>>,
    event_fetch: &Fetch,
    event_exists: &Exists,
) -> Result<StateMap<E::Id>>
where
    Fetch: Fn(E::Id) -> FetchFut + Sync,
    FetchFut: Future<Output = Option<E>> + Send,
    Exists: Fn(E::Id) -> ExistsFut,
    ExistsFut: Future<Output = bool> + Send,
    SetIter: Iterator<Item = &'a StateMap<E::Id>> + Clone + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Send + Sync,
    for<'b> &'b E: Send,
{
    debug!("State resolution starting");

    // Split non-conflicting and conflicting state
    let (clean, conflicting) = separate(state_sets.into_iter());

    debug!(count = clean.len(), "non-conflicting events");
    trace!(map = ?clean, "non-conflicting events");

    if conflicting.is_empty() {
        debug!("no conflicting state found");
        return Ok(clean);
    }

    debug!(count = conflicting.len(), "conflicting events");
    trace!(map = ?conflicting, "conflicting events");

    let auth_chain_diff =
        get_auth_chain_diff(&auth_chain_sets).chain(conflicting.into_values().flatten());

    // `all_conflicted` contains unique items
    // synapse says `full_set = {eid for eid in full_conflicted_set if eid in event_map}`
    let all_conflicted: HashSet<E::Id> = stream::iter(auth_chain_diff)
        // Don't honor events we cannot "verify"
        .filter(|id| event_exists(id.clone()))
        .collect()
        .await;

    debug!(count = all_conflicted.len(), "full conflicted set");
    trace!(set = ?all_conflicted, "full conflicted set");

    // We used to check that all events are events from the correct room
    // this is now a check the caller of `resolve` must make.

    // Get only the control events with a state_key: "" or ban/kick event (sender != state_key)
    let control_events = stream::iter(all_conflicted.iter())
        .filter(|&id| is_power_event_id(id, &event_fetch))
        .map(Clone::clone)
        .collect::<Vec<_>>()
        .await;

    // Sort the control events based on power_level/clock/event_id and outgoing/incoming edges
    let sorted_control_levels =
        reverse_topological_power_sort(control_events, &all_conflicted, &event_fetch).await?;

    debug!(count = sorted_control_levels.len(), "power events");
    trace!(list = ?sorted_control_levels, "sorted power events");

    let room_version = RoomVersion::new(room_version)?;
    // Sequentially auth check each control event.
    let resolved_control =
        iterative_auth_check(&room_version, &sorted_control_levels, clean.clone(), &event_fetch)
            .await?;

    debug!(count = resolved_control.len(), "resolved power events");
    trace!(map = ?resolved_control, "resolved power events");

    // At this point the control_events have been resolved we now have to
    // sort the remaining events using the mainline of the resolved power level.
    let deduped_power_ev = sorted_control_levels.into_iter().collect::<HashSet<_>>();

    // This removes the control events that passed auth and more importantly those that failed
    // auth
    let events_to_resolve = all_conflicted
        .iter()
        .filter(|&id| !deduped_power_ev.contains(id.borrow()))
        .cloned()
        .collect::<Vec<_>>();

    debug!(count = events_to_resolve.len(), "events left to resolve");
    trace!(list = ?events_to_resolve, "events left to resolve");

    // This "epochs" power level event
    let power_event = resolved_control.get(&(StateEventType::RoomPowerLevels, "".into()));

    debug!(event_id = ?power_event, "power event");

    let sorted_left_events =
        mainline_sort(&events_to_resolve, power_event.cloned(), &event_fetch).await?;

    trace!(list = ?sorted_left_events, "events left, sorted");

    let mut resolved_state = iterative_auth_check(
        &room_version,
        &sorted_left_events,
        resolved_control, // The control events are added to the final resolved state
        &event_fetch,
    )
    .await?;

    // Add unconflicted state to the resolved state
    // We priorities the unconflicting state
    resolved_state.extend(clean);

    debug!("state resolution finished");

    Ok(resolved_state)
}

/// Split the events that have no conflicts from those that are conflicting.
///
/// The return tuple looks like `(unconflicted, conflicted)`.
///
/// State is determined to be conflicting if for the given key (StateEventType, StateKey) there is
/// not exactly one event ID. This includes missing events, if one state_set includes an event that
/// none of the other have this is a conflicting event.
fn separate<'a, Id>(
    state_sets_iter: impl Iterator<Item = &'a StateMap<Id>> + Clone,
) -> (StateMap<Id>, StateMap<Vec<Id>>)
where
    Id: Clone + Eq + 'a,
{
    let mut unconflicted_state = StateMap::new();
    let mut conflicted_state = StateMap::new();

    for key in state_sets_iter.clone().flat_map(|map| map.keys()).unique() {
        let mut event_ids =
            state_sets_iter.clone().map(|state_set| state_set.get(key)).collect::<Vec<_>>();

        if event_ids.iter().all_equal() {
            // First .unwrap() is okay because
            // * event_ids has the same length as state_sets
            // * we never enter the loop this code is in if state_sets is empty
            let id = event_ids.pop().unwrap().expect("unconflicting `EventId` is not None");
            unconflicted_state.insert(key.clone(), id.clone());
        } else {
            conflicted_state
                .insert(key.clone(), event_ids.into_iter().filter_map(|o| o.cloned()).collect());
        }
    }

    (unconflicted_state, conflicted_state)
}

/// Returns a Vec of deduped EventIds that appear in some chains but not others.
fn get_auth_chain_diff<Id>(auth_chain_sets: &Vec<HashSet<Id>>) -> impl Iterator<Item = Id>
where
    Id: Clone + Eq + Hash,
{
    let num_sets = auth_chain_sets.len();
    let mut id_counts: HashMap<Id, usize> = HashMap::new();
    for id in auth_chain_sets.into_iter().flatten() {
        *id_counts.entry(id.clone()).or_default() += 1;
    }

    id_counts.into_iter().filter_map(move |(id, count)| (count < num_sets).then_some(id))
}

/// Events are sorted from "earliest" to "latest".
///
/// They are compared using the negative power level (reverse topological ordering), the origin
/// server timestamp and in case of a tie the `EventId`s are compared lexicographically.
///
/// The power level is negative because a higher power level is equated to an earlier (further back
/// in time) origin server timestamp.
#[instrument(level = "debug", skip_all)]
async fn reverse_topological_power_sort<E, F, Fut>(
    events_to_sort: Vec<E::Id>,
    auth_diff: &HashSet<E::Id>,
    fetch_event: &F,
) -> Result<Vec<E::Id>>
where
    F: Fn(E::Id) -> Fut + Sync,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Send + Sync,
{
    debug!("reverse topological sort of power events");

    let mut graph = HashMap::new();
    for event_id in events_to_sort {
        add_event_and_auth_chain_to_graph(&mut graph, event_id, auth_diff, fetch_event).await;

        // TODO: if these functions are ever made async here
        // is a good place to yield every once in a while so other
        // tasks can make progress
    }

    // This is used in the `key_fn` passed to the lexico_topo_sort fn
    let mut event_to_pl = HashMap::new();
    for event_id in graph.keys() {
        let pl = get_power_level_for_sender(event_id, fetch_event).await?;
        debug!(
            event_id = event_id.borrow().as_str(),
            power_level = i64::from(pl),
            "found the power level of an event's sender",
        );

        event_to_pl.insert(event_id.clone(), pl);

        // TODO: if these functions are ever made async here
        // is a good place to yield every once in a while so other
        // tasks can make progress
    }

    let event_to_pl = &event_to_pl;
    let fetcher = |event_id: E::Id| async move {
        let pl = *event_to_pl.get(event_id.borrow()).ok_or_else(|| Error::NotFound("".into()))?;
        let ev = fetch_event(event_id).await.ok_or_else(|| Error::NotFound("".into()))?;
        Ok((pl, ev.origin_server_ts()))
    };

    lexicographical_topological_sort(&graph, &fetcher).await
}

/// Sorts the event graph based on number of outgoing/incoming edges.
///
/// `key_fn` is used as to obtain the power level and age of an event for breaking ties (together
/// with the event ID).
#[instrument(level = "debug", skip_all)]
pub async fn lexicographical_topological_sort<Id, F, Fut>(
    graph: &HashMap<Id, HashSet<Id>>,
    key_fn: &F,
) -> Result<Vec<Id>>
where
    F: Fn(Id) -> Fut,
    Fut: Future<Output = Result<(Int, MilliSecondsSinceUnixEpoch)>> + Send,
    Id: Borrow<EventId> + Clone + Eq + Hash + Ord + Send,
{
    #[derive(Eq, Ord, PartialEq, PartialOrd)]
    struct TieBreaker<'a, Id> {
        inv_power_level: Int,
        age: MilliSecondsSinceUnixEpoch,
        event_id: &'a Id,
    }

    debug!("starting lexicographical topological sort");
    // NOTE: an event that has no incoming edges happened most recently,
    // and an event that has no outgoing edges happened least recently.

    // NOTE: this is basically Kahn's algorithm except we look at nodes with no
    // outgoing edges, c.f.
    // https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm

    // outdegree_map is an event referring to the events before it, the
    // more outdegree's the more recent the event.
    let mut outdegree_map = graph.clone();

    // The number of events that depend on the given event (the EventId key)
    // How many events reference this event in the DAG as a parent
    let mut reverse_graph: HashMap<_, HashSet<_>> = HashMap::new();

    // Vec of nodes that have zero out degree, least recent events.
    let mut zero_outdegree = Vec::new();

    for (node, edges) in graph {
        if edges.is_empty() {
            let (power_level, age) = key_fn(node.clone()).await?;
            // The `Reverse` is because rusts `BinaryHeap` sorts largest -> smallest we need
            // smallest -> largest
            zero_outdegree.push(Reverse(TieBreaker {
                inv_power_level: -power_level,
                age,
                event_id: node,
            }));
        }

        reverse_graph.entry(node).or_default();
        for edge in edges {
            reverse_graph.entry(edge).or_default().insert(node);
        }
    }

    let mut heap = BinaryHeap::from(zero_outdegree);

    // We remove the oldest node (most incoming edges) and check against all other
    let mut sorted = vec![];
    // Destructure the `Reverse` and take the smallest `node` each time
    while let Some(Reverse(item)) = heap.pop() {
        let node = item.event_id;

        for &parent in reverse_graph.get(node).expect("EventId in heap is also in reverse_graph") {
            // The number of outgoing edges this node has
            let out = outdegree_map
                .get_mut(parent.borrow())
                .expect("outdegree_map knows of all referenced EventIds");

            // Only push on the heap once older events have been cleared
            out.remove(node.borrow());
            if out.is_empty() {
                let (power_level, age) = key_fn(parent.clone()).await?;
                heap.push(Reverse(TieBreaker {
                    inv_power_level: -power_level,
                    age,
                    event_id: parent,
                }));
            }
        }

        // synapse yields we push then return the vec
        sorted.push(node.clone());
    }

    Ok(sorted)
}

/// Find the power level for the sender of `event_id` or return a default value of zero.
///
/// Do NOT use this any where but topological sort, we find the power level for the eventId
/// at the eventId's generation (we walk backwards to `EventId`s most recent previous power level
/// event).
async fn get_power_level_for_sender<E, F, Fut>(
    event_id: &E::Id,
    fetch_event: &F,
) -> serde_json::Result<Int>
where
    F: Fn(E::Id) -> Fut,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Send,
{
    debug!("fetch event ({event_id}) senders power level");

    let event = fetch_event(event_id.clone()).await;
    let mut pl = None;

    for aid in event.as_ref().map(|pdu| pdu.auth_events()).into_iter().flatten() {
        if let Some(aev) = fetch_event(aid.clone()).await {
            if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
                pl = Some(aev);
                break;
            }
        }
    }

    let content: PowerLevelsContentFields = match pl {
        None => return Ok(int!(0)),
        Some(ev) => from_json_str(ev.content().get())?,
    };

    if let Some(ev) = event {
        if let Some(&user_level) = content.users.get(ev.sender()) {
            debug!("found {} at power_level {user_level}", ev.sender());
            return Ok(user_level);
        }
    }

    Ok(content.users_default)
}

/// Check the that each event is authenticated based on the events before it.
///
/// ## Returns
///
/// The `unconflicted_state` combined with the newly auth'ed events. So any event that fails the
/// `event_auth::auth_check` will be excluded from the returned state map.
///
/// For each `events_to_check` event we gather the events needed to auth it from the the
/// `fetch_event` closure and verify each event using the `event_auth::auth_check` function.
async fn iterative_auth_check<E, F, Fut>(
    room_version: &RoomVersion,
    events_to_check: &[E::Id],
    unconflicted_state: StateMap<E::Id>,
    fetch_event: &F,
) -> Result<StateMap<E::Id>>
where
    F: Fn(E::Id) -> Fut,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Clone + Send,
    for<'a> &'a E: Send,
{
    debug!("starting iterative auth check");

    trace!(list = ?events_to_check, "events to check");

    let mut resolved_state = unconflicted_state;

    for event_id in events_to_check {
        let event = fetch_event(event_id.clone())
            .await
            .ok_or_else(|| Error::NotFound(format!("Failed to find {event_id}")))?;
        let state_key = event
            .state_key()
            .ok_or_else(|| Error::InvalidPdu("State event had no state key".to_owned()))?;

        let mut auth_events = StateMap::new();
        for aid in event.auth_events() {
            if let Some(ev) = fetch_event(aid.clone()).await {
                // TODO synapse check "rejected_reason" which is most likely
                // related to soft-failing
                auth_events.insert(
                    ev.event_type().with_state_key(ev.state_key().ok_or_else(|| {
                        Error::InvalidPdu("State event had no state key".to_owned())
                    })?),
                    ev,
                );
            } else {
                warn!(event_id = aid.borrow().as_str(), "missing auth event");
            }
        }

        for key in auth_types_for_event(
            event.event_type(),
            event.sender(),
            Some(state_key),
            event.content(),
        )? {
            if let Some(ev_id) = resolved_state.get(&key) {
                if let Some(event) = fetch_event(ev_id.clone()).await {
                    // TODO synapse checks `rejected_reason` is None here
                    auth_events.insert(key.to_owned(), event);
                }
            }
        }

        debug!("event to check {:?}", event.event_id());

        // The key for this is (eventType + a state_key of the signed token not sender) so
        // search for it
        let current_third_party = auth_events.iter().find_map(|(_, pdu)| {
            (*pdu.event_type() == TimelineEventType::RoomThirdPartyInvite).then_some(pdu)
        });

        let fetch_state = |ty: &StateEventType, key: &str| {
            future::ready(auth_events.get(&ty.with_state_key(key)))
        };

        if auth_check(room_version, &event, current_third_party, fetch_state).await? {
            // add event to resolved state map
            resolved_state.insert(event.event_type().with_state_key(state_key), event_id.clone());
        } else {
            // synapse passes here on AuthError. We do not add this event to resolved_state.
            warn!("event {event_id} failed the authentication check");
        }

        // TODO: if these functions are ever made async here
        // is a good place to yield every once in a while so other
        // tasks can make progress
    }
    Ok(resolved_state)
}

/// Returns the sorted `to_sort` list of `EventId`s based on a mainline sort using the depth of
/// `resolved_power_level`, the server timestamp, and the eventId.
///
/// The depth of the given event is calculated based on the depth of it's closest "parent"
/// power_level event. If there have been two power events the after the most recent are depth 0,
/// the events before (with the first power level as a parent) will be marked as depth 1. depth 1 is
/// "older" than depth 0.
async fn mainline_sort<E, F, Fut>(
    to_sort: &[E::Id],
    resolved_power_level: Option<E::Id>,
    fetch_event: &F,
) -> Result<Vec<E::Id>>
where
    F: Fn(E::Id) -> Fut,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Clone + Send,
{
    debug!("mainline sort of events");

    // There are no EventId's to sort, bail.
    if to_sort.is_empty() {
        return Ok(vec![]);
    }

    let mut mainline = vec![];
    let mut pl = resolved_power_level;
    while let Some(p) = pl {
        mainline.push(p.clone());

        let event = fetch_event(p.clone())
            .await
            .ok_or_else(|| Error::NotFound(format!("Failed to find {p}")))?;
        pl = None;
        for aid in event.auth_events() {
            let ev = fetch_event(aid.clone())
                .await
                .ok_or_else(|| Error::NotFound(format!("Failed to find {aid}")))?;
            if is_type_and_key(&ev, &TimelineEventType::RoomPowerLevels, "") {
                pl = Some(aid.to_owned());
                break;
            }
        }
        // TODO: if these functions are ever made async here
        // is a good place to yield every once in a while so other
        // tasks can make progress
    }

    let mainline_map = mainline
        .iter()
        .rev()
        .enumerate()
        .map(|(idx, eid)| ((*eid).clone(), idx))
        .collect::<HashMap<_, _>>();

    let mut order_map = HashMap::new();
    for ev_id in to_sort.iter() {
        if let Some(event) = fetch_event(ev_id.clone()).await {
            if let Ok(depth) = get_mainline_depth(Some(event), &mainline_map, fetch_event).await {
                order_map.insert(
                    ev_id,
                    (
                        depth,
                        fetch_event(ev_id.clone()).await.map(|ev| ev.origin_server_ts()),
                        ev_id,
                    ),
                );
            }
        }

        // TODO: if these functions are ever made async here
        // is a good place to yield every once in a while so other
        // tasks can make progress
    }

    // Sort the event_ids by their depth, timestamp and EventId
    // unwrap is OK order map and sort_event_ids are from to_sort (the same Vec)
    let mut sort_event_ids = order_map.keys().map(|&k| k.clone()).collect::<Vec<_>>();
    sort_event_ids.sort_by_key(|sort_id| order_map.get(sort_id).unwrap());

    Ok(sort_event_ids)
}

/// Get the mainline depth from the `mainline_map` or finds a power_level event that has an
/// associated mainline depth.
async fn get_mainline_depth<E, F, Fut>(
    mut event: Option<E>,
    mainline_map: &HashMap<E::Id, usize>,
    fetch_event: &F,
) -> Result<usize>
where
    F: Fn(E::Id) -> Fut,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Send,
{
    while let Some(sort_ev) = event {
        debug!(event_id = sort_ev.event_id().borrow().as_str(), "mainline");
        let id = sort_ev.event_id();
        if let Some(depth) = mainline_map.get(id.borrow()) {
            return Ok(*depth);
        }

        event = None;
        for aid in sort_ev.auth_events() {
            let aev = fetch_event(aid.clone())
                .await
                .ok_or_else(|| Error::NotFound(format!("Failed to find {aid}")))?;
            if is_type_and_key(&aev, &TimelineEventType::RoomPowerLevels, "") {
                event = Some(aev);
                break;
            }
        }
    }
    // Did not find a power level event so we default to zero
    Ok(0)
}

async fn add_event_and_auth_chain_to_graph<E, F, Fut>(
    graph: &mut HashMap<E::Id, HashSet<E::Id>>,
    event_id: E::Id,
    auth_diff: &HashSet<E::Id>,
    fetch_event: &F,
) where
    F: Fn(E::Id) -> Fut,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Clone + Send,
{
    let mut state = vec![event_id];
    while let Some(eid) = state.pop() {
        graph.entry(eid.clone()).or_default();
        // Prefer the store to event as the store filters dedups the events
        for aid in
            fetch_event(eid.clone()).await.as_ref().map(|ev| ev.auth_events()).into_iter().flatten()
        {
            if auth_diff.contains(aid.borrow()) {
                if !graph.contains_key(aid.borrow()) {
                    state.push(aid.to_owned());
                }

                // We just inserted this at the start of the while loop
                graph.get_mut(eid.borrow()).unwrap().insert(aid.to_owned());
            }
        }
    }
}

async fn is_power_event_id<E, F, Fut>(event_id: &E::Id, fetch: &F) -> bool
where
    F: Fn(E::Id) -> Fut,
    Fut: Future<Output = Option<E>> + Send,
    E: Event + Send,
    E::Id: Borrow<EventId> + Send,
{
    match fetch(event_id.clone()).await.as_ref() {
        Some(state) => is_power_event(state),
        _ => false,
    }
}

fn is_type_and_key(ev: impl Event, ev_type: &TimelineEventType, state_key: &str) -> bool {
    ev.event_type() == ev_type && ev.state_key() == Some(state_key)
}

fn is_power_event(event: impl Event) -> bool {
    match event.event_type() {
        TimelineEventType::RoomPowerLevels
        | TimelineEventType::RoomJoinRules
        | TimelineEventType::RoomCreate => event.state_key() == Some(""),
        TimelineEventType::RoomMember => {
            if let Ok(content) = from_json_str::<RoomMemberEventContent>(event.content().get()) {
                if [MembershipState::Leave, MembershipState::Ban].contains(&content.membership) {
                    return Some(event.sender().as_str()) != event.state_key();
                }
            }

            false
        }
        _ => false,
    }
}

/// Convenience trait for adding event type plus state key to state maps.
pub trait EventTypeExt {
    fn with_state_key(self, state_key: impl Into<String>) -> (StateEventType, String);
}

impl EventTypeExt for StateEventType {
    fn with_state_key(self, state_key: impl Into<String>) -> (StateEventType, String) {
        (self, state_key.into())
    }
}

impl EventTypeExt for TimelineEventType {
    fn with_state_key(self, state_key: impl Into<String>) -> (StateEventType, String) {
        (self.to_string().into(), state_key.into())
    }
}

impl<T> EventTypeExt for &T
where
    T: EventTypeExt + Clone,
{
    fn with_state_key(self, state_key: impl Into<String>) -> (StateEventType, String) {
        self.to_owned().with_state_key(state_key)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    };

    use js_int::{int, uint};
    use maplit::{hashmap, hashset};
    use rand::seq::SliceRandom;
    use ruma_common::{MilliSecondsSinceUnixEpoch, OwnedEventId, RoomVersionId};
    use ruma_events::{
        room::join_rules::{JoinRule, RoomJoinRulesEventContent},
        StateEventType, TimelineEventType,
    };
    use serde_json::{json, value::to_raw_value as to_raw_json_value};
    use tracing::debug;

    use crate::{
        is_power_event,
        room_version::RoomVersion,
        test_utils::{
            alice, bob, charlie, do_check, ella, event_id, member_content_ban, member_content_join,
            room_id, to_init_pdu_event, to_pdu_event, zara, PduEvent, TestStore, INITIAL_EVENTS,
        },
        Event, EventTypeExt, StateMap,
    };

    async fn test_event_sort() {
        use futures_util::future::ready;

        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());
        let events = INITIAL_EVENTS();

        let event_map = events
            .values()
            .map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.clone()))
            .collect::<StateMap<_>>();

        let auth_chain: HashSet<OwnedEventId> = HashSet::new();

        let power_events = event_map
            .values()
            .filter(|&pdu| is_power_event(&**pdu))
            .map(|pdu| pdu.event_id.clone())
            .collect::<Vec<_>>();

        let fetcher = |id| ready(events.get(&id).cloned());
        let sorted_power_events =
            crate::reverse_topological_power_sort(power_events, &auth_chain, &fetcher)
                .await
                .unwrap();

        let resolved_power = crate::iterative_auth_check(
            &RoomVersion::V6,
            &sorted_power_events,
            HashMap::new(), // unconflicted events
            &fetcher,
        )
        .await
        .expect("iterative auth check failed on resolved events");

        // don't remove any events so we know it sorts them all correctly
        let mut events_to_sort = events.keys().cloned().collect::<Vec<_>>();

        events_to_sort.shuffle(&mut rand::thread_rng());

        let power_level =
            resolved_power.get(&(StateEventType::RoomPowerLevels, "".to_owned())).cloned();

        let sorted_event_ids =
            crate::mainline_sort(&events_to_sort, power_level, &fetcher).await.unwrap();

        assert_eq!(
            vec![
                "$CREATE:foo",
                "$IMA:foo",
                "$IPOWER:foo",
                "$IJR:foo",
                "$IMB:foo",
                "$IMC:foo",
                "$START:foo",
                "$END:foo"
            ],
            sorted_event_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_sort() {
        for _ in 0..20 {
            // since we shuffle the eventIds before we sort them introducing randomness
            // seems like we should test this a few times
            test_event_sort().await;
        }
    }

    #[tokio::test]
    async fn ban_vs_power_level() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let events = &[
            to_init_pdu_event(
                "PA",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "MA",
                alice(),
                TimelineEventType::RoomMember,
                Some(alice().to_string().as_str()),
                member_content_join(),
            ),
            to_init_pdu_event(
                "MB",
                alice(),
                TimelineEventType::RoomMember,
                Some(bob().to_string().as_str()),
                member_content_ban(),
            ),
            to_init_pdu_event(
                "PB",
                bob(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
        ];

        let edges = vec![vec!["END", "MB", "MA", "PA", "START"], vec!["END", "PA", "PB"]]
            .into_iter()
            .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let expected_state_ids =
            vec!["PA", "MA", "MB"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(events, edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn topic_basic() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let events = &[
            to_init_pdu_event(
                "T1",
                alice(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "PA1",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "T2",
                alice(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "PA2",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 0 } })).unwrap(),
            ),
            to_init_pdu_event(
                "PB",
                bob(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "T3",
                bob(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
        ];

        let edges =
            vec![vec!["END", "PA2", "T2", "PA1", "T1", "START"], vec!["END", "T3", "PB", "PA1"]]
                .into_iter()
                .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
                .collect::<Vec<_>>();

        let expected_state_ids = vec!["PA2", "T2"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(events, edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn topic_reset() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let events = &[
            to_init_pdu_event(
                "T1",
                alice(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "PA",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "T2",
                bob(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "MB",
                alice(),
                TimelineEventType::RoomMember,
                Some(bob().to_string().as_str()),
                member_content_ban(),
            ),
        ];

        let edges = vec![vec!["END", "MB", "T2", "PA", "T1", "START"], vec!["END", "T1"]]
            .into_iter()
            .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let expected_state_ids =
            vec!["T1", "MB", "PA"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(events, edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn join_rule_evasion() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let events = &[
            to_init_pdu_event(
                "JR",
                alice(),
                TimelineEventType::RoomJoinRules,
                Some(""),
                to_raw_json_value(&RoomJoinRulesEventContent::new(JoinRule::Private)).unwrap(),
            ),
            to_init_pdu_event(
                "ME",
                ella(),
                TimelineEventType::RoomMember,
                Some(ella().to_string().as_str()),
                member_content_join(),
            ),
        ];

        let edges = vec![vec!["END", "JR", "START"], vec!["END", "ME", "START"]]
            .into_iter()
            .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let expected_state_ids = vec![event_id("JR")];

        do_check(events, edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn offtopic_power_level() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let events = &[
            to_init_pdu_event(
                "PA",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "PB",
                bob(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50, charlie(): 50 } }))
                    .unwrap(),
            ),
            to_init_pdu_event(
                "PC",
                charlie(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50, charlie(): 0 } }))
                    .unwrap(),
            ),
        ];

        let edges = vec![vec!["END", "PC", "PB", "PA", "START"], vec!["END", "PA"]]
            .into_iter()
            .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let expected_state_ids = vec!["PC"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(events, edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn topic_setting() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let events = &[
            to_init_pdu_event(
                "T1",
                alice(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "PA1",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "T2",
                alice(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "PA2",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 0 } })).unwrap(),
            ),
            to_init_pdu_event(
                "PB",
                bob(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
            ),
            to_init_pdu_event(
                "T3",
                bob(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "MZ1",
                zara(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
            to_init_pdu_event(
                "T4",
                alice(),
                TimelineEventType::RoomTopic,
                Some(""),
                to_raw_json_value(&json!({})).unwrap(),
            ),
        ];

        let edges = vec![
            vec!["END", "T4", "MZ1", "PA2", "T2", "PA1", "T1", "START"],
            vec!["END", "MZ1", "T3", "PB", "PA1"],
        ]
        .into_iter()
        .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
        .collect::<Vec<_>>();

        let expected_state_ids = vec!["T4", "PA2"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(events, edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn test_event_map_none() {
        use futures_util::future::ready;

        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let mut store = TestStore::<PduEvent>(hashmap! {});

        // build up the DAG
        let (state_at_bob, state_at_charlie, expected) = store.set_up();

        let ev_map = store.0.clone();
        let fetcher = |id| ready(ev_map.get(&id).cloned());

        let exists = |id: <PduEvent as Event>::Id| ready(ev_map.get(&*id).is_some());

        let state_sets = [state_at_bob, state_at_charlie];
        let auth_chain = state_sets
            .iter()
            .map(|map| store.auth_event_ids(room_id(), map.values().cloned().collect()).unwrap())
            .collect();

        let resolved =
            match crate::resolve(&RoomVersionId::V2, &state_sets, &auth_chain, &fetcher, &exists)
                .await
            {
                Ok(state) => state,
                Err(e) => panic!("{e}"),
            };

        assert_eq!(expected, resolved);
    }

    #[tokio::test]
    async fn test_lexicographical_sort() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());

        let graph = hashmap! {
            event_id("l") => hashset![event_id("o")],
            event_id("m") => hashset![event_id("n"), event_id("o")],
            event_id("n") => hashset![event_id("o")],
            event_id("o") => hashset![], // "o" has zero outgoing edges but 4 incoming edges
            event_id("p") => hashset![event_id("o")],
        };

        let res = crate::lexicographical_topological_sort(&graph, &|_id| async {
            Ok((int!(0), MilliSecondsSinceUnixEpoch(uint!(0))))
        })
        .await
        .unwrap();

        assert_eq!(
            vec!["o", "l", "n", "m", "p"],
            res.iter()
                .map(ToString::to_string)
                .map(|s| s.replace('$', "").replace(":foo", ""))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn ban_with_auth_chains() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());
        let ban = BAN_STATE_SET();

        let edges = vec![vec!["END", "MB", "PA", "START"], vec!["END", "IME", "MB"]]
            .into_iter()
            .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let expected_state_ids = vec!["PA", "MB"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(&ban.values().cloned().collect::<Vec<_>>(), edges, expected_state_ids).await;
    }

    #[tokio::test]
    async fn ban_with_auth_chains2() {
        use futures_util::future::ready;

        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());
        let init = INITIAL_EVENTS();
        let ban = BAN_STATE_SET();

        let mut inner = init.clone();
        inner.extend(ban);
        let store = TestStore(inner.clone());

        let state_set_a = [
            inner.get(&event_id("CREATE")).unwrap(),
            inner.get(&event_id("IJR")).unwrap(),
            inner.get(&event_id("IMA")).unwrap(),
            inner.get(&event_id("IMB")).unwrap(),
            inner.get(&event_id("IMC")).unwrap(),
            inner.get(&event_id("MB")).unwrap(),
            inner.get(&event_id("PA")).unwrap(),
        ]
        .iter()
        .map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
        .collect::<StateMap<_>>();

        let state_set_b = [
            inner.get(&event_id("CREATE")).unwrap(),
            inner.get(&event_id("IJR")).unwrap(),
            inner.get(&event_id("IMA")).unwrap(),
            inner.get(&event_id("IMB")).unwrap(),
            inner.get(&event_id("IMC")).unwrap(),
            inner.get(&event_id("IME")).unwrap(),
            inner.get(&event_id("PA")).unwrap(),
        ]
        .iter()
        .map(|ev| (ev.event_type().with_state_key(ev.state_key().unwrap()), ev.event_id.clone()))
        .collect::<StateMap<_>>();

        let ev_map = &store.0;
        let state_sets = [state_set_a, state_set_b];
        let auth_chain = state_sets
            .iter()
            .map(|map| store.auth_event_ids(room_id(), map.values().cloned().collect()).unwrap())
            .collect();

        let fetcher = |id: <PduEvent as Event>::Id| ready(ev_map.get(&id).cloned());
        let exists = |id: <PduEvent as Event>::Id| ready(ev_map.get(&id).is_some());
        let resolved =
            match crate::resolve(&RoomVersionId::V6, &state_sets, &auth_chain, &fetcher, &exists)
                .await
            {
                Ok(state) => state,
                Err(e) => panic!("{e}"),
            };

        debug!(
            resolved = ?resolved
                .iter()
                .map(|((ty, key), id)| format!("(({ty}{key:?}), {id})"))
                .collect::<Vec<_>>(),
                "resolved state",
        );

        let expected =
            ["$CREATE:foo", "$IJR:foo", "$PA:foo", "$IMA:foo", "$IMB:foo", "$IMC:foo", "$MB:foo"];

        for id in expected.iter().map(|i| event_id(i)) {
            // make sure our resolved events are equal to the expected list
            assert!(resolved.values().any(|eid| eid == &id) || init.contains_key(&id), "{id}");
        }
        assert_eq!(expected.len(), resolved.len());
    }

    #[tokio::test]
    async fn join_rule_with_auth_chain() {
        let join_rule = JOIN_RULE();

        let edges = vec![vec!["END", "JR", "START"], vec!["END", "IMZ", "START"]]
            .into_iter()
            .map(|list| list.into_iter().map(event_id).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let expected_state_ids = vec!["JR"].into_iter().map(event_id).collect::<Vec<_>>();

        do_check(&join_rule.values().cloned().collect::<Vec<_>>(), edges, expected_state_ids).await;
    }

    #[allow(non_snake_case)]
    fn BAN_STATE_SET() -> HashMap<OwnedEventId, Arc<PduEvent>> {
        vec![
            to_pdu_event(
                "PA",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
                &["CREATE", "IMA", "IPOWER"], // auth_events
                &["START"],                   // prev_events
            ),
            to_pdu_event(
                "PB",
                alice(),
                TimelineEventType::RoomPowerLevels,
                Some(""),
                to_raw_json_value(&json!({ "users": { alice(): 100, bob(): 50 } })).unwrap(),
                &["CREATE", "IMA", "IPOWER"],
                &["END"],
            ),
            to_pdu_event(
                "MB",
                alice(),
                TimelineEventType::RoomMember,
                Some(ella().as_str()),
                member_content_ban(),
                &["CREATE", "IMA", "PB"],
                &["PA"],
            ),
            to_pdu_event(
                "IME",
                ella(),
                TimelineEventType::RoomMember,
                Some(ella().as_str()),
                member_content_join(),
                &["CREATE", "IJR", "PA"],
                &["MB"],
            ),
        ]
        .into_iter()
        .map(|ev| (ev.event_id.clone(), ev))
        .collect()
    }

    #[allow(non_snake_case)]
    fn JOIN_RULE() -> HashMap<OwnedEventId, Arc<PduEvent>> {
        vec![
            to_pdu_event(
                "JR",
                alice(),
                TimelineEventType::RoomJoinRules,
                Some(""),
                to_raw_json_value(&json!({ "join_rule": "invite" })).unwrap(),
                &["CREATE", "IMA", "IPOWER"],
                &["START"],
            ),
            to_pdu_event(
                "IMZ",
                zara(),
                TimelineEventType::RoomPowerLevels,
                Some(zara().as_str()),
                member_content_join(),
                &["CREATE", "JR", "IPOWER"],
                &["START"],
            ),
        ]
        .into_iter()
        .map(|ev| (ev.event_id.clone(), ev))
        .collect()
    }
}

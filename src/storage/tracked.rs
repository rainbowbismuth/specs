use std::iter::Enumerate;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::slice;

use hibitset::{BitSet, BitSetLike};

use {Component, DenseVecStorage, Index, MaskedStorage, Storage, UnprotectedStorage};

#[derive(Copy, Clone, Debug, PartialEq, PartialOrd)]
pub enum Change {
    #[doc(hidden)]
    None,
    Inserted,
    Modified,
    Removed,
}

impl Change {
    fn add(&mut self, change: Change) {
        let new = match (*self, change) {
            (Change::None, new) => new,
            (Change::Inserted, Change::Modified) => Change::Inserted,
            (Change::Inserted, Change::Removed) => Change::None,
            (Change::Modified, Change::Modified) => Change::Modified,
            (Change::Modified, Change::Removed) => Change::Removed,
            (Change::Removed, Change::Inserted) => Change::Modified,
            (old, new) => panic!("Didn't expect change from {:?} to {:?}", old, new),
        };

        *self = new;
    }
}

pub struct ChangeEvents<'a> {
    inner: ChangeEventsInner<'a>,
}

impl<'a> Iterator for ChangeEvents<'a> {
    type Item = (Index, Change);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .find(|&(_, &c)| c != Change::None)
            .map(|(id, &c)| (id as Index, c))
    }
}

pub type ChangeEventsInner<'a> = Enumerate<slice::Iter<'a, Change>>;

impl<'e, S, T, D> Storage<'e, T, D>
where
    S: UnprotectedStorage<T> + Send + Sync + 'static,
    T: Component<Storage = TrackedStorage<T, S>> + Clone + Send + Sync,
    D: Deref<Target = MaskedStorage<T>>,
{
    /// Returns a bitset with all inserted and modified components added.
    /// This method is only provided if you're using `TrackedStorage`.
    pub fn changed_tracked(&self) -> &BitSet {
        self.data.inner.changed()
    }

    /// Returns an iterator over the change events generated by the `TrackedStorage`.
    pub fn change_events_tracked(&self) -> ChangeEvents {
        self.data.inner.change_events()
    }
}

impl<'e, S, T, D> Storage<'e, T, D>
where
    S: UnprotectedStorage<T> + Send + Sync + 'static,
    T: Component<Storage = TrackedStorage<T, S>> + Clone + Send + Sync,
    D: DerefMut<Target = MaskedStorage<T>>,
{
    /// Maintains the `TrackedStorage`.
    ///
    /// You can only call this in case your component implements `PartialEq`.
    /// This will compare the cache with the current storage and generate change events
    /// in case the `PartialEq` implementation says that two components are different.
    ///
    /// If you don't care about `Change::Modified` events, you don't have to call this method.
    ///
    /// ## When should I call this method?
    ///
    /// You should make sure that it gets called before you need the information which component
    /// has been modified. E.g. in case you have several systems writing to the component, then
    /// several reading from it, you can just call `maintain_tracked` once in between.
    pub fn maintain_tracked(&mut self)
    where
        T: PartialEq,
    {
        let (set, inner) = self.data.open_mut();
        unsafe {
            inner.maintain(set);
        }
    }

    /// Resets the tracked storage. This clears all change events and the `changed` bitset.
    /// You most likely want to do this at the end of every frame.
    pub fn reset_tracked(&mut self) {
        let (_, inner) = self.data.open_mut();
        unsafe {
            inner.reset();
        }
    }
}

#[derive(Derivative)]
#[derivative(Default(bound = "S: Default"))]
pub struct TrackedStorage<C, S = DenseVecStorage<C>> {
    /// All `Inserted` and `Changed` components are marked.
    changed: BitSet,
    changes: Vec<Change>,
    _marker: PhantomData<C>,
    old: S,
    storage: S,
}

impl<C, S> TrackedStorage<C, S>
where
    C: Clone,
    S: UnprotectedStorage<C>,
{
    /// Returns a reference to the `changed` bitset,
    /// which contains all components that have been inserted or modified
    /// since the last `reset`.
    pub fn changed(&self) -> &BitSet {
        &self.changed
    }

    pub fn change_events<'a>(&'a self) -> ChangeEvents<'a> {
        let inner = self.changes.iter().enumerate();

        ChangeEvents { inner }
    }

    unsafe fn reset(&mut self) {
        for id in &self.changed {
            let elem = self.old.get_mut(id);
            *elem = self.storage.get(id).clone();
        }

        self.changed.clear();
        self.changes.iter_mut().for_each(|c| *c = Change::None);
    }

    fn insert_change(changes: &mut Vec<Change>, id: Index, val: Change) {
        use std::cmp::max;
        use std::iter::repeat;

        let ind = id as usize;
        let len = changes.len();
        changes.extend(repeat(Change::None).take(max(ind + 1, len) - len));

        changes[ind].add(val);
    }
}

impl<C, S> TrackedStorage<C, S>
where
    C: Clone + PartialEq,
    S: UnprotectedStorage<C>,
{
    unsafe fn maintain(&mut self, set: &BitSet) {
        let TrackedStorage {
            ref old,
            ref storage,
            ref mut changes,
            ..
        } = *self;

        set.iter()
            .filter(|id| old.get(*id) != storage.get(*id))
            .for_each(|id| Self::insert_change(changes, id, Change::Modified))
    }
}

impl<C, S> UnprotectedStorage<C> for TrackedStorage<C, S>
where
    C: Clone,
    S: UnprotectedStorage<C>,
{
    unsafe fn clean<F>(&mut self, f: F)
    where
        F: Fn(Index) -> bool,
    {
        self.old.clean(&f);
        self.storage.clean(&f);
    }

    unsafe fn get(&self, id: Index) -> &C {
        self.storage.get(id)
    }

    unsafe fn get_mut(&mut self, id: Index) -> &mut C {
        self.storage.get_mut(id)
    }

    unsafe fn insert(&mut self, id: Index, value: C) {
        self.changed.add(id);
        Self::insert_change(&mut self.changes, id, Change::Inserted);

        self.old.insert(id, value.clone());
        self.storage.insert(id, value);
    }

    unsafe fn remove(&mut self, id: Index) -> C {
        // In case we marked this before, unmark it.
        self.changed.remove(id);
        Self::insert_change(&mut self.changes, id, Change::Removed);

        self.old.remove(id);
        self.storage.remove(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use World;

    #[derive(Clone, PartialEq)]
    struct Comp(u8);

    impl Component for Comp {
        type Storage = TrackedStorage<Self>;
    }

    fn world() -> World {
        let mut world = World::new();
        world.register::<Comp>();

        world
    }

    fn maint(w: &World) {
        w.write::<Comp>().maintain_tracked();
    }

    fn reset(w: &World) {
        w.write::<Comp>().reset_tracked();
    }

    macro_rules! ev_eq {
        ( $w:ident => $( $ent:ident : $change:expr )* ) => {
            {
                let comps = $w .read::<Comp>();
                assert_eq!(
                    comps.change_events_tracked().collect::<Vec<_>>(),
                    vec![$( ( $ent .id() , $change ), )*]
                );
            }
        };
    }

    #[test]
    fn insert() {
        let w = world();
        ev_eq!(w =>);

        let a = w.create_entity().with(Comp(1)).build();
        ev_eq!(w => a: Change::Inserted);
    }

    #[test]
    fn modified() {
        let w = world();
        let w = &w;
        ev_eq!(w =>);

        let a = w.create_entity().with(Comp(1)).build();
        ev_eq!(w => a: Change::Inserted);

        w.write().insert(a, Comp(2));
        ev_eq!(w => a: Change::Inserted);

        maint(w);
        ev_eq!(w => a: Change::Inserted);

        reset(w);
        ev_eq!(w =>);

        w.write().insert(a, Comp(4));
        maint(w);
        ev_eq!(w => a: Change::Modified);
    }

    #[test]
    fn removed_entity() {
        let mut w = world();
        let w = &mut w;

        let a = w.create_entity().with(Comp(0)).build();
        reset(w);
        ev_eq!(w =>);

        w.delete_entity(a).unwrap();
        ev_eq!(w => a: Change::Removed);
    }

    #[test]
    fn removed_entity_atomic() {
        let mut w = world();
        let w = &mut w;

        let a = w.create_entity().with(Comp(0)).build();
        reset(w);
        ev_eq!(w =>);

        w.entities().delete(a).unwrap();
        ev_eq!(w => );

        w.maintain();
        ev_eq!(w => a: Change::Removed);
    }

    #[test]
    fn removed_component() {
        let mut w = world();
        let w = &mut w;

        let a = w.create_entity().with(Comp(0)).build();
        reset(w);
        ev_eq!(w =>);

        w.write::<Comp>().remove(a);
        ev_eq!(w => a: Change::Removed);
    }

    #[test]
    fn remove_insert_mix() {
        let mut w = world();
        let w = &mut w;

        let a = w.create_entity().with(Comp(0)).build();
        reset(w);
        ev_eq!(w =>);

        w.write::<Comp>().remove(a);
        ev_eq!(w => a: Change::Removed);

        w.write::<Comp>().insert(a, Comp(1));
        maint(w);
        ev_eq!(w => a: Change::Modified);

        w.write::<Comp>().remove(a);
        ev_eq!(w => a: Change::Removed);

        reset(w);
        let b = w.create_entity().with(Comp(5)).build();
        ev_eq!(w => b: Change::Inserted);

        w.delete_entity(b).unwrap();
        ev_eq!(w =>);
    }

    #[test]
    fn join_changed() {
        use Join;

        let mut w = world();
        let w = &mut w;

        let a = w.create_entity().with(Comp(0)).build();
        let b = w.create_entity().with(Comp(1)).build();
        maint(w);

        let vec = w.read::<Comp>().changed_tracked().join().collect::<Vec<_>>();
        assert_eq!(vec, vec![a.id(), b.id()]);

        w.write().insert(a, Comp(10));
        let c = w.create_entity().with(Comp(2)).build();
        maint(w);

        let vec = w.read::<Comp>().changed_tracked().join().collect::<Vec<_>>();
        assert_eq!(vec, vec![a.id(), c.id()]);
    }
}

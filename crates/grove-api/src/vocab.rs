//! The declaration macro every fixture-pinned vocabulary in this crate is written in.
//!
//! A wire vocabulary has three renderings that must never disagree: the variants
//! themselves, the serde spelling, and the `ALL` list `tests/wire_vocab.rs` snapshots
//! into `contracts/wire-vocab.json`. Written by hand, the third one is the weak link —
//! `as_str`'s match is exhaustive so a new variant forces an arm *there*, but nothing
//! forces it into a hand-written `ALL`, and a guard test comparing the fixture group to
//! `ALL` is comparing the list to itself. A variant could ship, serialize, and reach a
//! client while the fixture that publishes this vocabulary never mentioned it — and
//! since no consumer reads that fixture, the omission would surface only as a value a
//! UI has no arm for.
//!
//! So the enum body *is* the list: [`wire_enum`] takes each variant beside its wire
//! spelling once and emits all three from that one line. Adding a variant means editing
//! the invocation, which is what puts it in `ALL` — and from there into the fixture,
//! where the snapshot guard fails until it is deliberately re-blessed.
//!
//! One cost, worth knowing before editing an invocation: rustfmt does not reformat the
//! inside of a macro call, so those declaration blocks are hand-aligned and `cargo fmt`
//! will not fix them. The blocks are one line per variant; keep them that way.

/// The number of idents given, as a const expression — the length of the `ALL` array.
macro_rules! count {
    () => (0usize);
    ($head:ident $($tail:ident)*) => (1usize + $crate::vocab::count!($($tail)*));
}

/// Declare a wire vocabulary: an enum plus its `ALL` list, `as_str`, and `Display`,
/// all derived from one `Variant => "spelling"` line each.
///
/// The spelling is a literal rather than a `rename_all` rule because the wire form is
/// contract and the Rust name is not: nothing should be able to change the former by
/// renaming the latter.
macro_rules! wire_enum {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident => $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(
            Clone, Copy, Debug, serde::Deserialize, Eq, Hash, PartialEq, serde::Serialize
        )]
        pub enum $name {
            $(
                $(#[$variant_meta])*
                #[serde(rename = $wire)]
                $variant,
            )+
        }

        impl $name {
            /// Every member of the vocabulary, in declaration order. This is what
            /// `contracts/wire-vocab.json` pins; it cannot omit a variant, because the
            /// variants and this list are the same text.
            pub const ALL: [Self; $crate::vocab::count!($($variant)+)] =
                [$(Self::$variant),+];

            /// The wire spelling — the same literal serde renames the variant to.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

pub(crate) use {count, wire_enum};

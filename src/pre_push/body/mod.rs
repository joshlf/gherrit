//! Pull-request body rendering during the owned-base activation transition.

mod legacy;
mod recipe;

pub(super) use legacy::PrBody;
#[allow(unused_imports)]
pub(super) use recipe::{
    BodyLinkContext, BodyRecipeInput, FinalBodyRecipes, GeneratedBody, RenderedBody,
    StackBodyRecipes,
};

// Per https://github.com/orgs/community/discussions/27190#discussioncomment-3254953,
// GitHub stores PR bodies in a `mediumblob` with a 262,144-byte limit. Use half
// of that limit as a safety factor.
pub(super) const MAX_BODY_SIZE_BYTES: usize = 131_072;

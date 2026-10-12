//! The pet's two configuration knobs, defined beside the rest of the
//! configuration model so validation and the knob registry own them;
//! the pet renderer consumes the resolved values.

/// Sprite variant: `"cat"` (orange tabby) or `"fox"` (red).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PetVariant {
    #[default]
    Cat,
    Fox,
}

impl PetVariant {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "cat" => Ok(Self::Cat),
            "fox" => Ok(Self::Fox),
            _ => Err(format!(
                "PetVariant must be one of: cat, fox (got {value:?})"
            )),
        }
    }
}

/// Energy dial: `"calm"` pets stroll and doze; `"rowdy"` pets dash more
/// and get hungry sooner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PetEnergy {
    #[default]
    Calm,
    Rowdy,
}

impl PetEnergy {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "calm" => Ok(Self::Calm),
            "rowdy" => Ok(Self::Rowdy),
            _ => Err(format!(
                "PetEnergy must be one of: calm, rowdy (got {value:?})"
            )),
        }
    }
}

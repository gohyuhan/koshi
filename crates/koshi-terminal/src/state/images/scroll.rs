//! Image movement and clipping when terminal rows scroll.

use super::*;

impl TerminalState {
    pub(in crate::state) fn scroll_image_rows(
        &mut self,
        first: u16,
        bottom: u16,
        shift: u16,
        up: bool,
        old_live_top: u64,
    ) {
        let primary = self.active == Screen::Primary;
        let new_live_top = if primary {
            self.scrollback.total_pushed()
        } else {
            0
        };
        let old_live_top = if primary { old_live_top } else { 0 };
        let full_history_scroll =
            primary && up && first == 0 && bottom + 1 == self.primary.dimensions().0;
        let placements = if primary {
            self.primary_absolute_image_placements_at(old_live_top)
        } else {
            std::mem::take(&mut self.alternate_image_placements)
                .into_iter()
                .filter_map(|placement| AbsoluteImagePlacement::from_live(placement, 0))
                .collect()
        };
        let (rows, columns) = self.active_grid().dimensions();
        let mut mapped = placements
            .into_iter()
            .filter_map(|mut placement| {
                if full_history_scroll || placement.anchor.0 < old_live_top {
                    return Some(placement);
                }
                let row = placement.anchor.0 - old_live_top;
                let end = row + u64::from(placement.rows);
                let contained = row >= u64::from(first) && end <= u64::from(bottom) + 1;
                if !contained {
                    placement.anchor.0 = new_live_top.checked_add(row)?;
                    return Some(placement);
                }
                if up {
                    let removed = u64::from(shift)
                        .saturating_sub(row - u64::from(first))
                        .min(u64::from(placement.rows));
                    if removed == u64::from(placement.rows) {
                        return None;
                    }
                    placement.plan.geometry.offset.y = placement
                        .plan
                        .geometry
                        .offset
                        .y
                        .checked_add(u16::try_from(removed).ok()?)?;
                    placement.rows -= u16::try_from(removed).ok()?;
                    placement.anchor.0 =
                        new_live_top.checked_add((row + removed).checked_sub(u64::from(shift))?)?;
                } else {
                    placement.anchor.0 = new_live_top.checked_add(row + u64::from(shift))?;
                    placement = placement.clipped(
                        new_live_top + u64::from(first),
                        new_live_top + u64::from(bottom) + 1,
                        columns,
                    )?;
                }
                Some(placement)
            })
            .collect::<Vec<_>>();
        if primary {
            self.set_primary_absolute_image_placements(&mut mapped);
        } else {
            self.alternate_image_placements = mapped
                .into_iter()
                .filter_map(|placement| {
                    placement.clipped(0, u64::from(rows), columns)?.into_live(0)
                })
                .collect();
        }
    }
}

#[cfg(test)]
mod tests;

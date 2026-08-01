//! Backend-neutral material batching plan. This module deliberately compiles on native too, so the
//! draw-count/performance fixture can run without a browser or GPU.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchItem {
    pub material: usize,
    pub blended: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BatchPlan {
    /// Source submesh indices grouped by opaque/masked material. Groups retain first-seen material
    /// order, making upload/draw order deterministic while reducing material binds and draw calls.
    pub opaque_groups: Vec<Vec<usize>>,
    /// Blended submeshes stay independent because they must be re-sorted back-to-front per frame.
    pub blended: Vec<usize>,
}

pub fn plan(items: &[BatchItem]) -> BatchPlan {
    let mut opaque_groups: Vec<Vec<usize>> = Vec::new();
    let mut material_groups: HashMap<usize, usize> = HashMap::new();
    let mut blended = Vec::new();

    for (index, item) in items.iter().enumerate() {
        if item.blended {
            blended.push(index);
            continue;
        }
        if let Some(group) = material_groups.get(&item.material) {
            opaque_groups[*group].push(index);
        } else {
            material_groups.insert(item.material, opaque_groups.len());
            opaque_groups.push(vec![index]);
        }
    }

    BatchPlan {
        opaque_groups,
        blended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_material_fixture_reduces_draws_without_batching_transparency() {
        // Representative CAD/game asset: 120 opaque objects sharing four materials plus six glass
        // panes. Before: 126 draws/material binds. After: four opaque batches + six ordered glass
        // draws = 10 (92% fewer draws). This CPU fixture makes the claimed measurement repeatable.
        let mut items: Vec<_> = (0..120)
            .map(|i| BatchItem {
                material: i % 4,
                blended: false,
            })
            .collect();
        items.extend((0..6).map(|_| BatchItem {
            material: 4,
            blended: true,
        }));

        let result = plan(&items);
        assert_eq!(result.opaque_groups.len(), 4);
        assert_eq!(result.blended.len(), 6);
        assert_eq!(
            result.opaque_groups.iter().map(Vec::len).sum::<usize>(),
            120
        );
        assert_eq!(result.opaque_groups.len() + result.blended.len(), 10);
    }
}

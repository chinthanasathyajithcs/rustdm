use crate::ipc::protocol::QueuePosition;

/// `order` with `id` moved to `position`, or `None` if `id` is not in it.
/// Moving the first entry up or the last one down leaves the order unchanged.
pub fn moved(order: &[String], id: &str, position: QueuePosition) -> Option<Vec<String>> {
    let from = order.iter().position(|x| x == id)?;
    let last = order.len() - 1;
    let to = match position {
        QueuePosition::Top => 0,
        QueuePosition::Up => from.saturating_sub(1),
        QueuePosition::Down => (from + 1).min(last),
        QueuePosition::Bottom => last,
    };

    let mut result: Vec<String> = order.iter().filter(|x| *x != id).cloned().collect();
    result.insert(to, id.to_string());
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn top_and_bottom_move_to_the_ends() {
        let o = order(&["a", "b", "c"]);
        assert_eq!(
            moved(&o, "c", QueuePosition::Top),
            Some(order(&["c", "a", "b"]))
        );
        assert_eq!(
            moved(&o, "a", QueuePosition::Bottom),
            Some(order(&["b", "c", "a"]))
        );
    }

    #[test]
    fn up_and_down_swap_with_a_neighbour() {
        let o = order(&["a", "b", "c"]);
        assert_eq!(
            moved(&o, "b", QueuePosition::Up),
            Some(order(&["b", "a", "c"]))
        );
        assert_eq!(
            moved(&o, "b", QueuePosition::Down),
            Some(order(&["a", "c", "b"]))
        );
    }

    #[test]
    fn moving_past_an_end_changes_nothing() {
        let o = order(&["a", "b"]);
        assert_eq!(moved(&o, "a", QueuePosition::Up), Some(o.clone()));
        assert_eq!(moved(&o, "b", QueuePosition::Down), Some(o.clone()));
    }

    #[test]
    fn unknown_ids_are_reported() {
        assert_eq!(moved(&order(&["a"]), "z", QueuePosition::Top), None);
        assert_eq!(moved(&[], "z", QueuePosition::Top), None);
    }
}

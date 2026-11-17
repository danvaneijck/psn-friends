use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Map `f` over `items` on up to `workers` threads, keeping input order.
pub fn par_map<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, items.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                let r = f(&items[i]);
                slots.lock().unwrap()[i] = Some(r);
            });
        }
    });
    slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("every slot filled"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_order() {
        let v: Vec<u32> = (0..50).collect();
        assert_eq!(
            super::par_map(&v, 4, |x| x * 2),
            (0..50).map(|x| x * 2).collect::<Vec<_>>()
        );
        assert!(super::par_map(&Vec::<u32>::new(), 4, |x| *x).is_empty());
    }
}

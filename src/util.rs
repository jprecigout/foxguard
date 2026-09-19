//! Petits utilitaires transverses, pas assez spécifiques à un module métier
//! pour y être rattachés.

use std::sync::{Mutex, MutexGuard};

/// Verrouille un [`Mutex`] en récupérant quand même les données s'il a été
/// empoisonné (un thread précédent a paniqué en le tenant), plutôt que de
/// propager la panique à l'appelant. FoxGuard n'a pas besoin d'invalider
/// l'état partagé (bounding-box, base de visages connus, ...) sur un panic
/// isolé : mieux vaut continuer avec la dernière valeur cohérente.
pub trait MutexExt<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_or_recover_behaves_like_lock_when_not_poisoned() {
        let mutex = Mutex::new(42);
        assert_eq!(*mutex.lock_or_recover(), 42);
    }

    #[test]
    fn lock_or_recover_returns_last_value_after_poisoning() {
        let mutex = Arc::new(Mutex::new(vec![1, 2, 3]));

        // On empoisonne volontairement le mutex : un thread panique en le
        // tenant, après l'avoir modifié.
        let poisoning = Arc::clone(&mutex);
        let result = std::thread::spawn(move || {
            let mut guard = poisoning.lock().unwrap();
            guard.push(4);
            panic!("panique volontaire pour empoisonner le mutex");
        })
        .join();
        assert!(result.is_err());
        assert!(mutex.is_poisoned());

        // lock_or_recover() doit quand même donner accès à la dernière
        // valeur cohérente plutôt que de propager la panique à l'appelant.
        let guard = mutex.lock_or_recover();
        assert_eq!(*guard, vec![1, 2, 3, 4]);
    }
}

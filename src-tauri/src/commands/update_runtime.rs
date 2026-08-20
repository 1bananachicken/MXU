//! 更新临界区与 Maa 运行时操作的门禁。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

const UPDATE_IN_PROGRESS: &str = "Update in progress";

/// 协调普通 Maa 运行时操作与更新临界区。
pub struct UpdateCoordinator {
    gate: Arc<RwLock<()>>,
    in_progress: AtomicBool,
}

impl Default for UpdateCoordinator {
    fn default() -> Self {
        Self {
            gate: Arc::new(RwLock::new(())),
            in_progress: AtomicBool::new(false),
        }
    }
}

impl UpdateCoordinator {
    /// 获取普通运行时操作许可；排队中的更新会优先进入临界区。
    pub async fn runtime_operation(self: &Arc<Self>) -> Result<OwnedRwLockReadGuard<()>, String> {
        if self.in_progress.load(Ordering::SeqCst) {
            return Err(UPDATE_IN_PROGRESS.to_string());
        }

        let permit = self.gate.clone().read_owned().await;
        if self.in_progress.load(Ordering::SeqCst) {
            return Err(UPDATE_IN_PROGRESS.to_string());
        }
        Ok(permit)
    }

    /// 同步命令使用的非阻塞运行时许可。
    pub fn try_runtime_operation(self: &Arc<Self>) -> Result<OwnedRwLockReadGuard<()>, String> {
        if self.in_progress.load(Ordering::SeqCst) {
            return Err(UPDATE_IN_PROGRESS.to_string());
        }

        let permit = self
            .gate
            .clone()
            .try_read_owned()
            .map_err(|_| UPDATE_IN_PROGRESS.to_string())?;
        if self.in_progress.load(Ordering::SeqCst) {
            return Err(UPDATE_IN_PROGRESS.to_string());
        }
        Ok(permit)
    }

    /// 等待所有正在启动的运行时操作完成后进入更新临界区。
    pub async fn begin_update(self: Arc<Self>) -> Result<UpdatePermit, String> {
        let permit = self.gate.clone().write_owned().await;
        self.in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "Update already in progress".to_string())?;

        Ok(UpdatePermit {
            coordinator: self,
            _permit: permit,
            keep_closed: false,
        })
    }

    pub fn is_update_in_progress(&self) -> bool {
        self.in_progress.load(Ordering::SeqCst)
    }
}

/// 更新临界区许可。失败路径默认在离开作用域时重新开放运行时。
pub struct UpdatePermit {
    coordinator: Arc<UpdateCoordinator>,
    _permit: OwnedRwLockWriteGuard<()>,
    keep_closed: bool,
}

impl UpdatePermit {
    /// 更新成功后保持关闭，直到当前进程退出。
    pub fn keep_runtime_closed(&mut self) {
        self.keep_closed = true;
    }
}

impl Drop for UpdatePermit {
    fn drop(&mut self) {
        if !self.keep_closed {
            self.coordinator.in_progress.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::UpdateCoordinator;

    #[test]
    fn failed_update_reopens_runtime_but_success_keeps_it_closed() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let coordinator = Arc::new(UpdateCoordinator::default());

        runtime.block_on(async {
            let update = coordinator.clone().begin_update().await.unwrap();
            assert!(coordinator.try_runtime_operation().is_err());
            drop(update);

            assert!(coordinator.try_runtime_operation().is_ok());

            let mut update = coordinator.clone().begin_update().await.unwrap();
            update.keep_runtime_closed();
            drop(update);

            assert!(coordinator.try_runtime_operation().is_err());
        });
    }
}

use async_trait::async_trait;
/**
 * SyncTaskExecutor 同步任务执行管理模块
 * 用于支持数据同步任务的执行和运行状态管理。
 *
 */
// use crate::{
//     application::synchronization::dtos::task_manager::CreateTaskManagerRequest,
//     domain::synchronization::sync_task::SyncTask,
//     infrastructure::mq::{
//         factory::{
//             create_tokio_broadcasting_channel, create_tokio_mpsc_channel, create_tokio_spmc_channel,
//         },
//         // message_bus::{MessageBusSender, StaticClonableMpscMQ},
//         tokio_channel_mq::{
//             TokioBroadcastingMessageBusReceiver, TokioBroadcastingMessageBusSender,
//             TokioMpscMessageBusReceiver, TokioMpscMessageBusSender, TokioSpmcMessageBusReceiver,
//             TokioSpmcMessageBusSender,
//         },
//     },
// };
use derivative::Derivative;
use getset::{Getters, MutGetters, Setters};
// use serde_json::Value;
use crate::{
    domain::synchronization::{
        rate_limiter::RateLimiter,
        sync_plan::SyncPlan,
        sync_task::SyncTask,
        task_executor::{SyncProgress, TaskExecutor, TaskExecutorError}, value_objects::sync_config::SyncMode,
    },
    infrastructure::{
        mq::{
            factory::create_tokio_broadcasting_channel,
            tokio_channel_mq::{
                TokioBroadcastingMessageBusReceiver, TokioBroadcastingMessageBusSender,
                TokioMpscMessageBusSender,
            },
        },
        sync::{
            factory::Builder,
            task_manager::{
                factory::{rate_limiter::RateLimiterBuilder, task_queue::TaskQueueBuilder},
                sync_rate_limiter::WebRequestRateLimiter,
                sync_task_queue::SyncTaskQueue,
                task_manager::TaskManager,
                task_queue::TaskQueue,
                tm_traits::SyncTaskManager,
            },
            workers::{
                worker::{WebAPISyncWorker, WebsocketSyncWorker},
                worker_traits::{LongRunningWorker, ShortRunningWorker}, factory::{http_worker_factory::create_http_worker, websocket_worker_factory::create_websocket_worker},
            },
            GetTaskRequest,
        },
    },
};
use std::collections::HashMap;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/**
 * Detailed Design
 *
 * By default, each sync plan is handled by one worker, but you can add more workers to each plan as long as it does not exceeds the
 * concurrency level limit.
 *
 * Primary Work Procedures
 * 1. Initialization
 *    - create data communication channels for workers and the task manager
 *    - create workers and the task manager, wire them up with channels created in the previous step
 * 2. Sync plan assignment
 *    - populates sync plans to task manager and wait for synchronization to start
 *    - assign workers to sync plans
 * 3. Continuous Synchronization
 *    - Set task manager and workers to running state
 * 4. Sync state management
 *    - run: start or continue a plan, ask workers to fetch tasks from task manager
 *    - pause: ask assigned workers to pause on given plan
 *    - cancel: ask workers to stop syncing the plan, and ask task manager to drop that plan
 *    - progress report: summarize current progress and return it
 */

#[derive(Derivative, Getters, Setters, MutGetters)]
#[getset(get = "pub", set = "pub")]
pub struct SyncTaskExecutor<LW, SW, TM> {
    idle_long_running_workers: HashMap<Uuid, LW>,
    busy_long_running_workers: HashMap<Uuid, LW>,
    idle_short_task_handling_workers: HashMap<Uuid, SW>,
    busy_short_task_handling_workers: HashMap<Uuid, SW>,
    task_manager: Arc<Mutex<TM>>,
    // worker_channels: WorkerChannels,
    // task_manager_channels: TaskManagerChannels,
}

impl<LW, SW, TM, TQ> SyncTaskExecutor<LW, SW, TM>
where
    TQ: TaskQueue,
    LW: LongRunningWorker,
    SW: ShortRunningWorker,
    TM: SyncTaskManager<TaskQueueType = TQ>,
{
    pub fn new() -> Self {
        todo!()
    }

    pub fn builder() -> Self {
        todo!()
    }

    // for simplicity, use tokio channel
    // may update to generic method to support multiple channel implementation
    fn allocate_task_request_channels(
        &self, 
        n_sync_plans: usize, 
        channel_size: Option<usize>
    ) -> (Vec<TokioBroadcastingMessageBusSender<GetTaskRequest>>, Vec<TokioBroadcastingMessageBusReceiver<GetTaskRequest>>) {
        let (task_sender, task_receiver) = create_tokio_broadcasting_channel::<GetTaskRequest>(channel_size.unwrap_or(200));
        
        let mut task_senders = Vec::with_capacity(n_sync_plans);
        let mut task_receivers = Vec::with_capacity(n_sync_plans);
        
        for _ in 0..n_sync_plans {
            task_senders.push(task_sender.clone());
            task_receivers.push(task_receiver.clone());
        }
    
        (task_senders, task_receivers)
    }
    
}

#[async_trait]
impl<LW, SW, TM, TQ> TaskExecutor for SyncTaskExecutor<LW, SW, TM>
where
    TQ: TaskQueue,
    LW: LongRunningWorker,
    SW: ShortRunningWorker,
    TM: SyncTaskManager<TaskQueueType = TQ>,
{
    type TaskManagerType = TM;
    type TaskQueueType = TQ;

    // add new sync plans to synchronize
    async fn assign(
        &mut self,
        sync_plans: Vec<Arc<RwLock<SyncPlan>>>,
    ) -> Result<(), TaskExecutorError>
    where
        <WebRequestRateLimiter as RateLimiter>::BuilderType:
            Builder<Product = WebRequestRateLimiter> + RateLimiterBuilder + Send,
        <Self::TaskQueueType as TaskQueue>::BuilderType: Builder<Product = Self::TaskQueueType>
            + TaskQueueBuilder<
                RateLimiterType = WebRequestRateLimiter,
                TaskRequestReceiverType = TokioBroadcastingMessageBusReceiver<GetTaskRequest>,
            > + Send,
        <Self::TaskManagerType as SyncTaskManager>::TaskQueueType: TaskQueue,
        <Self as TaskExecutor>::TaskManagerType: SyncTaskManager,
        <Self as TaskExecutor>::TaskQueueType: TaskQueue,
    {
        let (
            task_senders, 
            task_receivers) = self.allocate_task_request_channels(sync_plans.len(), None);
        // Lock the task manager and load the sync plans
        let mut task_manager_lock = self.task_manager.lock().await;
        
        for sync_plan in sync_plans {
            let plan_lock = sync_plan.read().await;
            match plan_lock.sync_config().sync_mode() {
                SyncMode::HttpAPI => {
                    // Fixme: fill the correct type
                    let http_worker = create_http_worker(task_request_sender, todo_task_receiver, completed_task_sender, failed_task_sender);
                    self.idle_short_task_handling_workers.insert(*http_worker.id(), http_worker);
                },
                SyncMode::WebsocketStreaming => {
                    let websocket_worker = create_websocket_worker(task_request_sender, todo_task_receiver, completed_task_sender, failed_task_sender);
                    self.idle_long_running_workers.insert(*websocket_worker.id(), websocket_worker);
                }
            }
        }

        match task_manager_lock.load_sync_plans::<WebRequestRateLimiter, TokioBroadcastingMessageBusReceiver<GetTaskRequest>>(sync_plans, task_receivers).await {
            Ok(_) => Ok(()),
            Err(e) => { Err(TaskExecutorError::LoadPlanFailure) }, // Convert the error type if needed
        }

        // Allocate workers for each sync plan
    }

    // run a single plan. Either start a new plan or continue a paused plan
    async fn run(&mut self, sync_plan_id: Uuid) -> Result<(), TaskExecutorError> {
        todo!()
    }

    // run all assigned plans
    async fn run_all(&mut self) -> Result<(), TaskExecutorError> {
        todo!()
    }

    // temporarily pause a plan
    async fn pause(&mut self, sync_plan_id: Uuid) -> Result<(), TaskExecutorError> {
        todo!()
    }

    // pause all plans
    async fn pause_all(&mut self) -> Result<(), TaskExecutorError> {
        todo!()
    }

    // cancel sync for plan, also removes it from the executor
    async fn cancel(&mut self, sync_plan_id: Uuid) -> Result<(), TaskExecutorError> {
        todo!()
    }

    // cancel and drop all plans
    async fn cancel_all(&mut self) -> Result<(), TaskExecutorError> {
        todo!()
    }

    // report current progress
    async fn report_progress(&self) -> Result<SyncProgress, TaskExecutorError> {
        todo!()
    }
}

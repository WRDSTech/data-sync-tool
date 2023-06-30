//! Task Manager
//! Defines synchronization task queue and a manager module to support task polling, scheduling and throttling.
//!

use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    ops::RangeBounds,
    sync::Arc,
};

use derivative::Derivative;
use futures::future::join_all;
use getset::{Getters, Setters};

use tokio::{join, sync::Mutex, task::JoinHandle};
use uuid::Uuid;

use crate::{
    domain::synchronization::{
        custom_errors::TimerError,
        rate_limiter::{RateLimitStatus, RateLimiter},
        sync_task::SyncTask,
    },
    infrastructure::mq::message_bus::MessageBus,
};

type DatasetId = Uuid;
type CooldownTimerTask = JoinHandle<()>;
type TimeSecondLeft = i64;

pub enum SyncTaskQueueValue {
    Task(Option<Arc<Mutex<SyncTask>>>),
    RateLimited(Option<CooldownTimerTask>, TimeSecondLeft), // timer task, seco
    DailyLimitExceeded,
}

#[derive(Derivative, Getters, Setters, Debug)]
#[getset(get = "pub", set = "pub")]
pub struct SyncTaskQueue<T: RateLimiter> {
    tasks: Mutex<VecDeque<Arc<Mutex<SyncTask>>>>,
    rate_limiter: Option<T>,
}

impl<T: RateLimiter> SyncTaskQueue<T> {
    pub fn new(tasks: Vec<Arc<Mutex<SyncTask>>>, rate_limiter: Option<T>) -> SyncTaskQueue<T> {
        let task_queue = Mutex::new(VecDeque::from(tasks));
        if let Some(rate_limiter) = rate_limiter {
            SyncTaskQueue {
                tasks: task_queue,
                rate_limiter: Some(rate_limiter),
            }
        } else {
            SyncTaskQueue {
                tasks: task_queue,
                rate_limiter: None,
            }
        }
    }

    pub async fn pop_front(&mut self) -> Result<SyncTaskQueueValue, TimerError> {
        //! try to pop the front of the task queue
        //! if the queue is empty, or the queue has a rate limiter, and the rate limiter rejects the request, return None
        let mut q_lock = self.tasks.lock().await;
        match &mut self.rate_limiter {
            Some(rate_limiter) => {
                let rate_limiter_response = rate_limiter.can_proceed().await;
                match rate_limiter_response {
                    RateLimitStatus::Ok(available_request_left) => {
                        println!(
                            "Rate limiter permits this request. There are {} requests left.",
                            available_request_left
                        );
                        let value = q_lock.pop_front();
                        if let Some(value) = value {
                            return Ok(SyncTaskQueueValue::Task(Some(value.clone())));
                        } else {
                            return Ok(SyncTaskQueueValue::Task(None));
                        }
                    }
                    RateLimitStatus::RequestPerDayExceeded => {
                        return Ok(SyncTaskQueueValue::DailyLimitExceeded);
                    }
                    RateLimitStatus::RequestPerMinuteExceeded(
                        should_start_cooldown,
                        seconds_left,
                    ) => {
                        if should_start_cooldown {
                            let countdown_task = rate_limiter.start_countdown(true).await?;
                            return Ok(SyncTaskQueueValue::RateLimited(
                                Some(countdown_task),
                                seconds_left,
                            ));
                        } else {
                            return Ok(SyncTaskQueueValue::RateLimited(None, seconds_left));
                        }
                    }
                }
            }
            None => {
                let value = q_lock.pop_front();
                if let Some(value) = value {
                    return Ok(SyncTaskQueueValue::Task(Some(value.clone())));
                } else {
                    return Ok(SyncTaskQueueValue::Task(None));
                }
            }
        }
    }

    pub async fn drain<R: RangeBounds<usize>>(&mut self, range: R) -> Vec<Arc<Mutex<SyncTask>>> {
        //! Pops all elements in the queue given the range
        //! Typically used when the remote reports a daily limited reached error
        let mut q_lock = self.tasks.lock().await;
        let values = q_lock.drain(range);
        return values.collect::<Vec<_>>();
    }

    pub async fn push_back(&mut self, task: Arc<Mutex<SyncTask>>) {
        let mut q_lock = self.tasks.lock().await;
        q_lock.push_back(task);
        return ();
    }

    pub async fn push_front(&mut self, task: Arc<Mutex<SyncTask>>) {
        let mut q_lock = self.tasks.lock().await;
        q_lock.push_front(task);
        return ();
    }

    pub async fn front(&self) -> Option<Arc<Mutex<SyncTask>>> {
        let q_lock = self.tasks.lock().await;
        q_lock.front().cloned()
    }

    pub async fn is_empty(&self) -> bool {
        let q_lock = self.tasks.lock().await;
        return q_lock.is_empty();
    }

    pub async fn len(&self) -> usize {
        let q_lock = self.tasks.lock().await;
        return q_lock.len();
    }
}

#[derive(Debug)]
pub enum TaskManagerError {
    RateLimited(Option<CooldownTimerTask>, TimeSecondLeft), // timer task, seco
    DailyLimitExceeded,
}

type MaxRetry = u32;

/// TaskManager
#[derive(Derivative, Getters, Setters)]
#[getset(get = "pub", set = "pub")]
pub struct TaskManager<T, MT, ME, MF>
where
    T: RateLimiter,
    MT: MessageBus<Arc<Mutex<SyncTask>>>,
    ME: MessageBus<TaskManagerError>,
    MF: MessageBus<(DatasetId, Arc<Mutex<SyncTask>>)> + std::marker::Send,
{
    queues: Arc<Mutex<HashMap<DatasetId, (Arc<Mutex<SyncTaskQueue<T>>>, MaxRetry)>>>,
    task_channel: Arc<Mutex<MT>>,
    error_message_channel: Arc<Mutex<ME>>,
    failed_task_channel: Arc<Mutex<MF>>,
}

impl<T, MT, ME, MF> TaskManager<T, MT, ME, MF>
where
    T: RateLimiter + 'static,
    MT: MessageBus<Arc<Mutex<SyncTask>>>,
    ME: MessageBus<TaskManagerError>,
    MF: MessageBus<(DatasetId, Arc<Mutex<SyncTask>>)> + std::marker::Send + 'static,
{
    pub fn new(
        task_queues: Arc<Mutex<HashMap<DatasetId, (Arc<Mutex<SyncTaskQueue<T>>>, MaxRetry)>>>,
        task_channel: Arc<Mutex<MT>>,
        error_message_channel: Arc<Mutex<ME>>,
        failed_task_channel: Arc<Mutex<MF>>,
    ) -> TaskManager<T, MT, ME, MF> {
        Self {
            queues: task_queues,
            task_channel,
            error_message_channel,
            failed_task_channel,
        }
    }

    pub async fn add_queue(&mut self, dataset_id: DatasetId, task_queue: SyncTaskQueue<T>, max_retry: MaxRetry) {
        let mut qs_lock = self.queues.lock().await;
        qs_lock.insert(dataset_id, (Arc::new(Mutex::new(task_queue)), max_retry));
    }

    pub async fn add_tasks(&mut self, tasks: Vec<SyncTask>) {
        let mut q_lock = self.queues.lock().await;

        for task in tasks {
            let dataset_id = task.dataset_id();
            if let Some(dataset_id) = dataset_id {
                let task_queue = q_lock.get_mut(dataset_id);
                if let Some((task_queue, _)) = task_queue {
                    let mut task_queue_lock = task_queue.lock().await;
                    task_queue_lock.push_back(Arc::new(Mutex::new(task))).await;
                }
            }
        }
    }

    /// Check whether all queues are empty. If so, the
    pub async fn all_queues_are_empty(&self) -> bool {
        let queues: Vec<_> = self.queues.lock().await.values().cloned().collect();
        let all_queues_empty = join_all(queues.into_iter().map(|queue| async move {
            let queue = queue.0.lock().await;
            queue.is_empty().await
        }))
        .await;
        let result = all_queues_empty.into_iter().all(|r| r);

        return result;
    }

    pub async fn stop(&self) -> Result<(), Box<dyn Error>> {
        let task_channel_lock = self.task_channel.lock().await;
        let error_message_channel_lock = self.error_message_channel.lock().await;
        let failed_task_channel = self.failed_task_channel.lock().await;

        let _ = join!(
            task_channel_lock.close(),
            error_message_channel_lock.close(),
            failed_task_channel.close()
        );

        return Ok(());
    }

    /// start task manager and push tasks to its consumers
    /// Task manager will poll its queues and try to get a task from each of them, and then send the task to task channel
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let queues = self.queues.clone();
        let failed_task_channel = Arc::clone(&self.failed_task_channel);
        let handle_failures = tokio::spawn(async move {
            while let Ok(Some((dataset_id, failed_task))) =
                failed_task_channel.lock().await.receive().await
            {
                if let Some((queue, retries_left)) = queues.lock().await.get_mut(&dataset_id) {
                    let mut queue_lock = queue.lock().await;
                    if *retries_left > 0 {
                        queue_lock.push_back(failed_task).await;
                        *retries_left -= 1;
                    }
                }
            }
        });

        loop {
            // Check whether all queues are empty
            // break the loop if all queues are empty
            if self.all_queues_are_empty().await {
                println!("All task queues are empty. Exit.");
                // Should I close all channels after exiting? Probably I should.
                self.stop().await?;

                break;
            }

            let mut any_task_found = false;
            for (dataset_id, task_queue) in &mut self.queues.lock().await.iter_mut() {
                let mut q_lock = task_queue.0.lock().await;
                let task_value = q_lock.pop_front().await?;
                // pull tasks from queues and send it to consumers
                match task_value {
                    SyncTaskQueueValue::Task(t) => {
                        if let Some(t) = t {
                            // Send the task to its consumer
                            let task_channel_lock = self.task_channel.lock().await;
                            let _ = task_channel_lock.send(t).await;
                            any_task_found = true;
                        } else {
                            println!("Received no task from queue {}!", dataset_id);
                        }
                    }
                    SyncTaskQueueValue::RateLimited(cooldown_task, time_left) => {
                        let error_message_channel_lock = self.error_message_channel.lock().await;
                        let _ = error_message_channel_lock
                            .send(TaskManagerError::RateLimited(cooldown_task, time_left))
                            .await;
                    }
                    SyncTaskQueueValue::DailyLimitExceeded => {
                        // tell task manager's consumers that daily limit is triggered
                        // TODO: figure out what to do with it? Does task manager just tell others this error or do something further?
                        let error_message_channel_lock = self.error_message_channel.lock().await;
                        let _ = error_message_channel_lock
                            .send(TaskManagerError::DailyLimitExceeded)
                            .await;
                    }
                }
            }

            // TODO: handle failed tasks, push them back to queue

            if !any_task_found {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        let _ = handle_failures.await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::synchronization::value_objects::task_spec::{TaskSpecification, RequestMethod},
        infrastructure::{
            sync::sync_rate_limiter::WebRequestRateLimiter,
            mq::tokio_channel_mq::TokioMpscMessageBus},
    };

    use super::*;
    use std::sync::Arc;

    use chrono::Local;
    use fake::faker::internet::en::SafeEmail;
    use fake::faker::name::en::Name;
    use fake::Fake;
    use rand::Rng;
    use rbatis::dark_std::errors::new;
    use serde_json::Value;
    use url::Url;

    fn random_string(len: usize) -> String {
        let mut rng = rand::thread_rng();
        std::iter::repeat(())
            .map(|()| rng.sample(rand::distributions::Alphanumeric))
            .map(char::from)
            .take(len)
            .collect()
    }

    pub fn generate_random_sync_tasks(n: u32) -> Vec<SyncTask> {
        (0..n).map(|_| {
            let fake_url = format!("http://{}", SafeEmail().fake::<String>());
            let request_endpoint = Url::parse(&fake_url).unwrap();
            let fake_headers: HashMap<String, String> = (0..5).map(|_| (Name().fake::<String>(), random_string(20))).collect();
            let fake_payload = Some(Arc::new(Value::String(random_string(50))));
            let fake_method = if rand::random() { RequestMethod::Get } else { RequestMethod::Post };
            let task_spec = TaskSpecification::new(&fake_url, if fake_method == RequestMethod::Get { "GET" } else { "POST" }, fake_headers, fake_payload).unwrap();

            let start_time = Local::now();
            let create_time = Local::now();
            let dataset_name = Some(random_string(10));
            let datasource_name = Some(random_string(10));
            SyncTask::new(
                Uuid::new_v4(),
                &dataset_name.unwrap(),
                Uuid::new_v4(),
                &datasource_name.unwrap(),
                task_spec,
                Uuid::new_v4()
            )
        }).collect()
    }

    #[test]
    fn it_should_generate_random_tasks(){
        let tasks = generate_random_sync_tasks(10);
        assert!(tasks.len() == 10);
        // println!("{:?}", tasks);

    }

    #[tokio::test]
    async fn test_add_tasks_to_a_single_queue() {
        // Arrange
        // TODO: reduce the layers of Arc<Mutex<T>>
        let test_rate_limiter = WebRequestRateLimiter::new(30, None, Some(3)).unwrap();
        // let task_channel = Arc::new(Mutex::new(TokioMpscMessageBus::<Arc<Mutex<SyncTask>>>::new(200)));
        // let error_message_channel = Arc::new(Mutex::new(TokioMpscMessageBus::<TaskManagerError>::new(200)));
        // let failed_task_channel = Arc::new(Mutex::new(TokioMpscMessageBus::<(Uuid, Arc<Mutex<SyncTask>>)>::new(200)));
        // let task_manager = Arc::new(Mutex::new(TaskManager::new(
        //     task_queue, 
        //     task_channel, 
        //     error_message_channel,
        //     failed_task_channel)));
        let task_queue: Arc<Mutex<SyncTaskQueue<WebRequestRateLimiter>>> = Arc::new(Mutex::new(
            SyncTaskQueue::<WebRequestRateLimiter>::new(vec![], Some(test_rate_limiter))
        ));

        let tasks = generate_random_sync_tasks(100);
        let first_task = tasks[0].clone();

        let mut task_queue_lock = task_queue.lock().await;
        
        for t in tasks {
            task_queue_lock.push_back(Arc::new(Mutex::new(t))).await;
            println!("task queue size: {}", task_queue_lock.len().await);
        }

        if let Some(t1) = task_queue_lock.front().await {
            let t_lock = t1.lock().await;
            assert_eq!(t_lock.id(), first_task.id(), "Task id not equal!")
        }
        // task_queue_lock.p

        // Act
        // let mut task_manager_lock = task_manager.lock().await;
        // task_manager_lock.add_tasks(tasks).await;

        // // Assert
        // let task_queue_lock = task_manager_lock.queues.lock().await;
        // let task_queue = task_queue_lock.get(&first_task.dataset_id().unwrap());
        // println!("Keys: {:?}", task_queue_lock.keys());

        // if let Some(q) = task_queue {
        //     println!("Queue: {:?}", q);
        // }
        
        // assert!(
        //     task_queue.is_some(),
        //     "No task queue for the given dataset_id"
        // );

        // let task_queue = task_queue.unwrap();
        // let (queue, _) = task_queue;
        // let queue_lock = queue.lock().await;
        // assert_eq!(queue_lock.len().await, 1, "The task was not added to the queue");
        // let task_in_queue = queue_lock.front().await.unwrap();
        // assert_eq!(
        //     task_in_queue.lock().await.id(),
        //     first_task.id(),
        //     "The task in the queue is not the one that was added"
        // );
    }
}
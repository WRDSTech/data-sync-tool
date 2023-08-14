# SyncTaskExecutor Design & Implementation

## Overview

`SyncTaskExecutor` is the component responsible for managing the execution of sync tasks. It collaborates closely with workers to fetch data for tasks, as well as TaskManager to schedule and track the status of tasks.

## Design Goal

From a high level perspective, the design goal of SyncTaskExecutor is to accept a 2d list of SyncTasks and ask workers to fetch and fill data for these task objects. Downstream modules will then take the data and store them in designated storage. SyncTaskExecutor should be flexible enough to provide a continuous task execution feature. This means users may add new SyncPlan and execute them immediately or manually start a SyncPlan even though the plan is not due yet. Users can also stop a executing SyncPlan without interrupting other running tasks. It should also handle errors gracefully, and provide friendly error message. Even if the error is unrecoverable, it should wait for all tasks to complete, or stop all running tasks gracefully.

## Functional Requirement

1. Be able to pull data by coordinating Workers and TaskManager
2. Can flexiblely add, remove, and stop tasks while running
3. Handles errors gracefully

## Structural Design

#### Data Members

* `workers_pool`: A dynamic pool of workers, where each worker is responsible for a specific type of data synchronization (HTTP request, WebSocket connection, etc.). The pool can scale up and down depending on the load of tasks.
* `task_manager`: Responsible for scheduling and tracking tasks. It keeps a queue of pending tasks and can move tasks between different statuses (Pending, Running, Finished, Failed).
* `task_queue`: A queue where tasks are pulled from for execution. This queue can be dynamically added and removed from, offering flexibility for immediate or manual execution of tasks.

#### Methods

* `add_task(sync_plan: SyncPlan)`: This method adds a new sync plan to the task queue. It allows users to dynamically add tasks for immediate or later execution.
* `remove_task(task_id: Uuid)`: This method removes a task from the task queue using its unique identifier. It's used when a task is no longer needed.
* `start_task(task_id: Uuid)`: This method immediately starts a task, regardless of its planned schedule. It provides flexibility in executing tasks as needed.
* `stop_task(task_id: Uuid)`: This method gracefully stops a running task. It ensures that the task stops without interrupting other tasks.
* `execute()`: This is the main method that continuously pulls tasks from the task queue and assigns them to available workers in the pool. It handles task execution and error recovery, making sure that tasks are executed in a robust manner.

### Error Handling

Error handling is done in a graceful manner, providing friendly error messages for users. Unrecoverable errors are handled in a way that allows all tasks to either complete or be stopped gracefully. To achieve this, each worker communicates with the `SyncTaskExecutor` through a dedicated error message channel. When a worker encounters an error, it sends an error message to the executor. The executor then determines the severity of the error and either attempts to retry the task, moves the task to a failed status, or, in the worst case, stops the entire execution process.

### Concurrent Task Execution

`SyncTaskExecutor` is designed to handle multiple tasks concurrently. Each task is assigned to a worker, which operates independently from other workers. This design allows for high throughput and efficient use of resources. It also enables the executor to stop individual tasks without affecting others.

### Task Scheduling and Management

Task scheduling is handled by the `TaskManager`, which maintains a queue of tasks and their statuses. The `SyncTaskExecutor` interacts with the `TaskManager` to get the next task to be executed, update the status of tasks, and handle failed tasks.

### Flexibility

`SyncTaskExecutor` is designed with flexibility in mind. It allows users to add new tasks for immediate execution or schedule them for later. It also provides the capability to start tasks manually or stop them as needed. This flexibility allows the executor to adapt to changing requirements and usage patterns.


## Implementation

Right now I really have to think about the implementation. My rough idea of how an implementation of SyncTaskManager could work is as follows:

1. Client modules call load_sync_plan or load_sync_plans to add one or many sync_plans to TaskManager. Inside the task manager, the task manager will create new SyncTaskQueue for each sync plan and load their sync task to the queue. I guess these two methods also need to add additional parameters for SyncTaskQueue's RateLimiter if the rate of sending tasks needs to be throttled.
2. Then the client module will call start_sending_all_tasks to send tasks over its channels.
3. Meanwhile, some other modules may add tasks to sync plans' queues while the task manager is sending tasks from the queue.
4. They may also pause, resume or stop a sync task's queue.
5. They may also call progress reporting methods.
6. Task sending must not stop unless the task manager is asked to pause, stop, or shutdown. Thus, task sending must work concurrently along with other method calls.
7. Finally, when all tasks are sent, task manager will go to sleep until the next load_sync_plans and start_sending_all_tasks is called.
8. When graceful_shutdown is called, all unsent tasks will be dropped, and task manager will cancel all running tasks. Then it will close its channels
9. When force_shutdown is called, task manager will immediately cancel all running tasks without waiting. Then it will close its channels.

SyncTaskManager is an important dependency of the SyncTaskExecutor. We can safely assume that SyncTaskManager is primarily used by SyncTaskExecutor.
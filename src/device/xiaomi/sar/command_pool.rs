use std::collections::VecDeque;

use super::QueuedData;

/// 双队列：一个存放 L1 CMD，另一个存放带序号的 L1 Data
/// 之所以拆分是为了让 CMD 不受窗口限制优先发送
#[derive(Default)]
pub struct CommandPool {
    /// 命令队列（无需 seq）
    cmd_queue: VecDeque<Vec<u8>>,
    /// 数据队列（已分配 seq）
    data_queue: VecDeque<QueuedData>,
}

impl CommandPool {
    pub fn new() -> Self {
        Self {
            cmd_queue: VecDeque::new(),
            data_queue: VecDeque::new(),
        }
    }

    /// 普通数据入队（追加到末尾）
    pub fn push(&mut self, data: QueuedData) {
        self.data_queue.push_back(data);
    }

    /// 普通数据入队（插到队首）
    pub fn push_front(&mut self, data: QueuedData) {
        self.data_queue.push_front(data);
    }

    /// 批量插队：如将 6,7,8 插到 1,2,3 前应先压 8,7,6 到队首
    pub fn extend_front<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = QueuedData>,
    {
        let mut items: Vec<QueuedData> = iter.into_iter().collect();
        while let Some(d) = items.pop() {
            self.data_queue.push_front(d);
        }
    }

    /// 取出一个待发送的数据包
    pub fn pop_data(&mut self) -> Option<QueuedData> {
        self.data_queue.pop_front()
    }

    /// 推入一条 CMD（永远插队到队首）
    pub fn push_cmd_front(&mut self, cmd: Vec<u8>) {
        self.cmd_queue.push_front(cmd);
    }

    /// 取出一条 CMD
    pub fn pop_cmd(&mut self) -> Option<Vec<u8>> {
        self.cmd_queue.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.cmd_queue.is_empty() && self.data_queue.is_empty()
    }
}

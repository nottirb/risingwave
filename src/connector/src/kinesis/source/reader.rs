// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::Arc;
use std::{thread, time};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use aws_sdk_kinesis::error::GetRecordsError;
use aws_sdk_kinesis::model::ShardIteratorType;
use aws_sdk_kinesis::output::GetRecordsOutput;
use aws_sdk_kinesis::types::SdkError;
use aws_sdk_kinesis::Client as KinesisClient;
use aws_smithy_types::DateTime;
use tokio::sync::Mutex;

use crate::base::{SourceMessage, SplitReader};
use crate::kinesis::source::message::KinesisMessage;
use crate::kinesis::source::state::KinesisSplitReaderState;
use crate::kinesis::split::{KinesisOffset, KinesisSplit};
use crate::{ConnectorStateV2, KinesisProperties};
use crate::kinesis::{build_client, KINESIS_STREAM_NAME};

pub struct KinesisSplitReader {
    client: KinesisClient,
    stream_name: String,
    shard_id: String,
    latest_sequence_num: String,
    shard_iter: Option<String>,
    assigned_split: Option<KinesisSplit>,
}

pub struct KinesisMultiSplitReader {
    client: KinesisClient,
    // splits are not allowed to be empty, otherwise connector source should create
    // [`DummySplitReader`] which is always idling.
    splits: Vec<KinesisSplit>,
    shard_iter: Arc<Mutex<HashMap<String, String>>>,
}

#[async_trait]
impl SplitReader for KinesisSplitReader {
    async fn next(&mut self) -> Result<Option<Vec<SourceMessage>>> {
        loop {
            let iter = match &self.shard_iter {
                Some(_iter) => _iter,
                None => {
                    return Err(anyhow::Error::msg(format!(
                        "invalid shard iter, shard_id {}",
                        self.shard_id
                    )));
                }
            };
            let get_record_output = match self.get_records(iter.clone()).await {
                Ok(record_resp) => record_resp,
                Err(SdkError::DispatchFailure(e)) => {
                    return Err(anyhow!(e));
                }
                Err(SdkError::ServiceError { err, .. }) if err.is_expired_iterator_exception() => {
                    match self.renew_shard_iter().await {
                        Ok(_) => {}
                        Err(e) => {
                            return Err(e);
                        }
                    }
                    return self.next().await;
                }
                Err(SdkError::ServiceError { err, .. })
                    if err.is_provisioned_throughput_exceeded_exception() =>
                {
                    return Err(anyhow::Error::msg(err));
                }
                Err(e) => {
                    return Err(anyhow!("{}", e));
                }
            };
            println!("get_record_output {:#?}", get_record_output);
            self.shard_iter = get_record_output.next_shard_iterator.clone();

            let records = get_record_output.records.unwrap_or_default();
            if records.is_empty() {
                // if records is empty, retry after 200ms to avoid
                // ProvisionedThroughputExceededException
                thread::sleep(time::Duration::from_millis(200));
                continue;
            }

            let mut record_collection: Vec<SourceMessage> = Vec::new();
            for record in records {
                if !is_stopping(
                    record.sequence_number.as_ref().unwrap(),
                    self.assigned_split.as_ref().unwrap(),
                ) {
                    return Ok(Some(record_collection));
                }
                self.latest_sequence_num = record.sequence_number().unwrap().to_string();
                record_collection.push(SourceMessage::from(KinesisMessage::new(
                    self.shard_id.clone(),
                    record,
                )));
            }
            return Ok(Some(record_collection));
        }
    }
}

impl KinesisSplitReader {
    /// For Kinesis, state identifier is `split_id`, `stream_name` is never changed
    pub async fn new(config: KinesisProperties, state: ConnectorStateV2) -> Result<Self>
    where
        Self: Sized,
    {
        let client = build_client(&config).await?;

        let mut split_reader = KinesisSplitReader {
            client,
            stream_name: config.get(KINESIS_STREAM_NAME)?,
            shard_id: String::from(""),
            latest_sequence_num: "".to_string(),
            shard_iter: None,
            assigned_split: None,
        };

        if let ConnectorStateV2::State(state) = state {
            let split_id = String::from_utf8(state.identifier.to_vec())?;

            let mut start_offset = KinesisOffset::Earliest;
            if !state.start_offset.is_empty() {
                start_offset = KinesisOffset::SequenceNumber(state.start_offset);
            }
            let mut end_offset = KinesisOffset::None;
            if !state.end_offset.is_empty() {
                end_offset = KinesisOffset::SequenceNumber(state.end_offset);
            }
            let split = KinesisSplit {
                shard_id: split_id.clone(),
                start_position: start_offset.clone(),
                end_position: end_offset.clone(),
            };

            let shard_iter: Option<String> = match &start_offset {
                KinesisOffset::Earliest => {
                    Self::get_kinesis_iterator(
                        &split_reader.client,
                        &split_reader.stream_name,
                        &split_id,
                        ShardIteratorType::TrimHorizon,
                        None,
                        None,
                    )
                    .await?
                }
                KinesisOffset::SequenceNumber(seq_number) => {
                    Self::get_kinesis_iterator(
                        &split_reader.client,
                        &split_reader.stream_name,
                        &split_id,
                        ShardIteratorType::AfterSequenceNumber,
                        None,
                        Some(seq_number.clone()),
                    )
                    .await?
                }
                other => {
                    return Err(anyhow::Error::msg(format!("invalid KinesisOffset, expect either KinesisOffset::Earliest or KinesisOffset::SequenceNumber, got {:?}", other)));
                }
            };

            split_reader.assigned_split = Some(split);
            split_reader.shard_iter = shard_iter;
        } else if let ConnectorStateV2::Splits(s) = state {
        } else {
            unreachable!()
        }

        Ok(split_reader)
    }
}

impl KinesisSplitReader {
    async fn get_records(
        &self,
        shard_iter: String,
    ) -> core::result::Result<GetRecordsOutput, SdkError<GetRecordsError>> {
        let resp = self
            .client
            .get_records()
            .shard_iterator(shard_iter.clone())
            .send()
            .await;
        resp
    }

    async fn renew_shard_iter(&mut self) -> Result<()> {
        let get_shard_iter_resp = self
            .client
            .get_shard_iterator()
            .stream_name(&self.stream_name)
            .shard_id(self.shard_id.clone())
            .shard_iterator_type(aws_sdk_kinesis::model::ShardIteratorType::AfterSequenceNumber)
            .starting_sequence_number(self.latest_sequence_num.clone())
            .send()
            .await;
        self.shard_iter = match get_shard_iter_resp {
            Ok(resp) => resp.shard_iterator().map(String::from),
            Err(e) => {
                return Err(anyhow!("{}", e));
            }
        };
        Ok(())
    }

    async fn get_kinesis_iterator(
        client: &KinesisClient,
        stream_name: &String,
        shard_id: &String,
        shard_iterator_type: aws_sdk_kinesis::model::ShardIteratorType,
        timestamp: Option<i64>,
        seq_num: Option<String>,
    ) -> Result<Option<String>> {
        let mut get_shard_iter_req = client
            .get_shard_iterator()
            .stream_name(stream_name)
            .shard_id(shard_id)
            .shard_iterator_type(shard_iterator_type);

        if let Some(ts) = timestamp {
            get_shard_iter_req = get_shard_iter_req.set_timestamp(Some(DateTime::from_secs(ts)));
        }
        if let Some(seq) = seq_num {
            get_shard_iter_req = get_shard_iter_req.set_starting_sequence_number(Some(seq));
        }

        let get_shard_iter_resp = get_shard_iter_req.send().await;
        match get_shard_iter_resp {
            Ok(resp) => return Ok(resp.shard_iterator().map(String::from)),
            Err(e) => {
                return Err(anyhow!("{}", e));
            }
        };
    }

    fn get_state(&self) -> KinesisSplitReaderState {
        KinesisSplitReaderState::new(
            self.stream_name.clone(),
            self.shard_id.clone(),
            self.latest_sequence_num.clone(),
        )
    }

    async fn restore_from_state(&mut self, state: KinesisSplitReaderState) -> Result<()> {
        self.stream_name = state.stream_name;
        self.shard_id = state.shard_id;
        self.latest_sequence_num = state.sequence_number;

        self.renew_shard_iter().await
    }
}

fn is_stopping(cur_seq_num: &str, split: &KinesisSplit) -> bool {
    match &split.end_position {
        KinesisOffset::SequenceNumber(stopping_seq_num) => {
            if cur_seq_num < stopping_seq_num.as_str() {
                return true;
            }
            false
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use async_stream::stream;
    use std::error::Error;
    use rand::Rng;
    use futures_async_stream::{for_await, try_stream};
    use futures_concurrency::prelude::*;

    use super::*;

    #[try_stream(ok = i32, error = anyhow::Error)]
    async fn stream(i: i32, sleep: u64) {
        loop {
            yield i;
            std::thread::sleep(std::time::Duration::from_millis(sleep));
    }

    #[tokio::test]
    async fn test_stream() -> Result<()> {
        // let s_1 = stream! {
        //     let mut i = 0;
        //     loop {
        //         yield Ok(i);
        //         i += 1;
        //         std::thread::sleep(std::time::Duration::from_millis(300))
        //     }
        // };
        // let s_2 = stream! {
        //     let mut i = 100;
        //     loop {
        //         yield Ok(i);
        //         i += 1;
        //         std::thread::sleep(std::time::Duration::from_millis(500))
        //     }
        // };
        // let s_3 = stream! {
        //     let mut i = 1000;
        //     loop {
        //         yield Ok(i);
        //         i += 1;
        //         std::thread::sleep(std::time::Duration::from_millis(700))
        //     }
        // };

        let s_1 = stream(0, 300);
        let s_2 = stream(100, 500);
        let s_3 = stream(1000, 700);

        let s = (s_1, s_2, s_3).merge().into_stream();

        #[for_await]
        for msg in s {
            println!("{:?}", msg);
        }

        Ok(())
    }
}

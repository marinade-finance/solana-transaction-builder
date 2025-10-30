[1mdiff --git a/.gitignore b/.gitignore[m
[1mindex d6010c7..85731f9 100644[m
[1m--- a/.gitignore[m
[1m+++ b/.gitignore[m
[36m@@ -1,3 +1,5 @@[m
 target[m
 [m
[31m-.idea/[m
\ No newline at end of file[m
[32m+[m[32m.idea/[m
[32m+[m
[32m+[m[32mnode_modules/[m
[1mdiff --git a/libs/solana-transaction-builder-executor/src/builder_executor.rs b/libs/solana-transaction-builder-executor/src/builder_executor.rs[m
[1mindex ad2ff71..8e12d72 100644[m
[1m--- a/libs/solana-transaction-builder-executor/src/builder_executor.rs[m
[1m+++ b/libs/solana-transaction-builder-executor/src/builder_executor.rs[m
[36m@@ -74,7 +74,7 @@[m [mpub async fn execute_transaction_data_in_sequence([m
     let sequence_length = execution_data.len();[m
     let mut errors = TransactionBuilderExecutionErrors::new();[m
 [m
[31m-    for (index, async_transaction_builder) in execution_data.into_iter().enumerate() {[m
[32m+[m[32m    for (index, async_transaction_builder) in execution_data.iter().enumerate() {[m
         let human_index = index + 1;[m
         let tx_uuid = &async_transaction_builder.tx_uuid;[m
         debug!("Building the transaction {human_index}/{tx_uuid} (size: {sequence_length})");[m
[36m@@ -126,7 +126,7 @@[m [mpub async fn execute_transaction_data_in_parallel([m
 [m
     // Prepare the list of futures with their associated tx_uuid and human_index[m
     let futures = execution_data[m
[31m-        .into_iter()[m
[32m+[m[32m        .iter()[m
         .enumerate()[m
         .map(|(index, async_transaction_builder)| {[m
             let human_index = index + 1;[m
[1mdiff --git a/libs/solana-transaction-builder-executor/src/error.rs b/libs/solana-transaction-builder-executor/src/error.rs[m
[1mindex ebd95ec..737b966 100644[m
[1m--- a/libs/solana-transaction-builder-executor/src/error.rs[m
[1m+++ b/libs/solana-transaction-builder-executor/src/error.rs[m
[36m@@ -45,6 +45,12 @@[m [mpub struct TransactionBuilderExecutionErrors {[m
     errors: Vec<TransactionBuilderExecutionError>,[m
 }[m
 [m
[32m+[m[32mimpl Default for TransactionBuilderExecutionErrors {[m
[32m+[m[32m    fn default() -> Self {[m
[32m+[m[32m        Self::new()[m
[32m+[m[32m    }[m
[32m+[m[32m}[m
[32m+[m
 impl TransactionBuilderExecutionErrors {[m
     pub fn new() -> Self {[m
         TransactionBuilderExecutionErrors { errors: vec![] }[m

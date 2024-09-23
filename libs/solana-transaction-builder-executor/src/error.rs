use std::fmt;

#[derive(Debug)]
pub struct TransactionBuilderExecutionError {
    pub cause: anyhow::Error,
    pub tx_uuid: String,
    pub human_index: usize,
    pub description: String,
}

impl TransactionBuilderExecutionError {
    pub fn new(
        cause: anyhow::Error,
        tx_uuid: String,
        human_index: usize,
        description: String,
    ) -> Self {
        TransactionBuilderExecutionError {
            cause,
            tx_uuid,
            human_index,
            description,
        }
    }
}

impl fmt::Display for TransactionBuilderExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TransactionBuilderExecutionError: tx_uuid: {}, human_index: {}, description: {}, cause: {}",
            self.tx_uuid, self.human_index, self.description, self.cause
        )
    }
}

impl std::error::Error for TransactionBuilderExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.cause)
    }
}

#[derive(Debug)]
pub struct TransactionBuilderExecutionErrors {
    errors: Vec<TransactionBuilderExecutionError>,
}

impl TransactionBuilderExecutionErrors {
    pub fn new() -> Self {
        TransactionBuilderExecutionErrors { errors: vec![] }
    }

    pub fn add_error_instance(&mut self, error: TransactionBuilderExecutionError) {
        self.errors.push(error);
    }

    pub fn add_error(
        &mut self,
        cause: anyhow::Error,
        tx_uuid: String,
        human_index: usize,
        description: String,
    ) {
        self.errors.push(TransactionBuilderExecutionError::new(
            cause,
            tx_uuid,
            human_index,
            description,
        ));
    }

    pub fn len(&self) -> usize {
        self.errors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &TransactionBuilderExecutionError> {
        self.errors.iter()
    }
}

impl IntoIterator for TransactionBuilderExecutionErrors {
    type Item = TransactionBuilderExecutionError;
    type IntoIter = std::vec::IntoIter<TransactionBuilderExecutionError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.into_iter()
    }
}

impl fmt::Display for TransactionBuilderExecutionErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransactionBuilderExecutionErrors:")?;
        for error in self.iter() {
            write!(f, "\n{}", error)?;
        }
        Ok(())
    }
}

impl std::error::Error for TransactionBuilderExecutionErrors {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.errors
            .first()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

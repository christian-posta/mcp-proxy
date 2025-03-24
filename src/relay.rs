use crate::rbac;
use crate::xds::{Target, TargetSpec, XdsStore};
use rmcp::ClientHandlerService;
use rmcp::serve_client;
use rmcp::service::RunningService;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::sse::SseTransport;
use rmcp::{
	Error as McpError, RoleServer, ServerHandler, model::CallToolRequestParam, model::Tool, model::*,
	service::RequestContext,
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct Relay {
	state: Arc<std::sync::RwLock<XdsStore>>,
	pool: Arc<RwLock<ConnectionPool>>,
	id: rbac::Identity,
}

impl Relay {
	pub fn new(state: Arc<std::sync::RwLock<XdsStore>>, id: rbac::Identity) -> Self {
		Self {
			state: state.clone(),
			pool: Arc::new(RwLock::new(ConnectionPool::new(state.clone()))),
			id,
		}
	}
}

// TODO: lists and gets can be macros
impl ServerHandler for Relay {
	fn get_info(&self) -> ServerInfo {
		ServerInfo {
      protocol_version: ProtocolVersion::V_2024_11_05,
      capabilities: ServerCapabilities {
          experimental: None,
          logging: None,
          prompts: Some(PromptsCapability::default()),
          resources: Some(ResourcesCapability::default()),
          tools: Some(ToolsCapability {
              list_changed: None,
          }),
      },
      server_info: Implementation::from_build_env(),
			instructions: Some(
				"This server provides a counter tool that can increment and decrement values. The counter starts at 0 and can be modified using the 'increment' and 'decrement' tools. Use 'get_value' to check the current count.".to_string(),
			),
		}
	}

	async fn list_resources(
		&self,
		request: PaginatedRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListResourcesResult, McpError> {
		let pool = self.pool.read().await;
		let all = pool.iter().await.map(|(_name, svc)| {
			let svc = svc.clone();
			let request = request.clone();
			async move {
				let result = svc
					.as_ref()
					.read()
					.await
					.list_resources(request)
					.await
					.unwrap();
				result.resources
			}
		});

		Ok(ListResourcesResult {
			resources: futures::future::join_all(all)
				.await
				.into_iter()
				.flatten()
				.collect(),
			next_cursor: None,
		})
	}

	async fn read_resource(
		&self,
		request: ReadResourceRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ReadResourceResult, McpError> {
		if !self.state.read().unwrap().policies.validate(
			&rbac::ResourceType::Resource {
				id: request.uri.to_string(),
			},
			&self.id,
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}
		let pool = self.pool.read().await;
		let target = pool.get(&request.uri).await.unwrap();
		let result = target
			.as_ref()
			.read()
			.await
			.read_resource(request)
			.await
			.unwrap();

		Ok(ReadResourceResult {
			contents: result.contents,
		})
	}

	async fn list_resource_templates(
		&self,
		request: PaginatedRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListResourceTemplatesResult, McpError> {
		let pool = self.pool.read().await;
		let all = pool.iter().await.map(|(_name, svc)| {
			let svc = svc.clone();
			let request = request.clone();
			async move {
				let result = svc
					.as_ref()
					.read()
					.await
					.list_resource_templates(request)
					.await
					.unwrap();
				result.resource_templates
			}
		});

		Ok(ListResourceTemplatesResult {
			resource_templates: futures::future::join_all(all)
				.await
				.into_iter()
				.flatten()
				.collect(),
			next_cursor: None,
		})
	}

	async fn list_prompts(
		&self,
		request: PaginatedRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListPromptsResult, McpError> {
		let pool = self.pool.read().await;
		let all = pool.iter().await.map(|(_name, svc)| {
			let svc = svc.clone();
			let request = request.clone();
			async move {
				let result = svc
					.as_ref()
					.read()
					.await
					.list_prompts(request)
					.await
					.unwrap();
				result.prompts
			}
		});

		Ok(ListPromptsResult {
			prompts: futures::future::join_all(all)
				.await
				.into_iter()
				.flatten()
				.collect(),
			next_cursor: None,
		})
	}

	async fn get_prompt(
		&self,
		request: GetPromptRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<GetPromptResult, McpError> {
		if !self.state.read().unwrap().policies.validate(
			&rbac::ResourceType::Prompt {
				id: request.name.to_string(),
			},
			&self.id,
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}
		let tool_name = request.name.to_string();
		let (service_name, tool) = tool_name.split_once(':').unwrap();
		let pool = self.pool.read().await;
		let service = pool.get(service_name).await.unwrap();
		let req = GetPromptRequestParam {
			name: tool.to_string(),
			arguments: request.arguments,
		};

		let result = service.as_ref().read().await.get_prompt(req).await.unwrap();
		Ok(result)
	}

	async fn list_tools(
		&self,
		request: PaginatedRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListToolsResult, McpError> {
		let mut tools = Vec::new();
		// TODO: Use iterators
		// TODO: Handle individual errors
		// TODO: Do we want to handle pagination here, or just pass it through?
		tracing::info!("listing tools");
		for (name, service) in self.pool.read().await.iter().await {
			tracing::info!("listing tools for target: {}", name);
			let result = service
				.as_ref()
				.read()
				.await
				.list_tools(request.clone())
				.await
				.unwrap();
			tracing::info!("result: {:?}", result);
			for tool in result.tools {
				let tool_name = format!("{}:{}", name, tool.name);
				tracing::info!("tool: {}", tool_name);
				tools.push(Tool {
					name: Cow::Owned(tool_name),
					description: tool.description,
					input_schema: tool.input_schema,
				});
			}
		}
		Ok(ListToolsResult {
			tools,
			next_cursor: None,
		})
	}

	async fn call_tool(
		&self,
		request: CallToolRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<CallToolResult, McpError> {
		tracing::info!("calling tool: {:?}", request);
		if !self.state.read().unwrap().policies.validate(
			&rbac::ResourceType::Tool {
				id: request.name.to_string(),
			},
			&self.id,
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}
		let tool_name = request.name.to_string();
		let (service_name, tool) = tool_name.split_once(':').unwrap();
		let pool = self.pool.read().await;
		let service = pool.get(service_name).await.unwrap();
		let req = CallToolRequestParam {
			name: Cow::Owned(tool.to_string()),
			arguments: request.arguments,
		};

		let result = service.as_ref().read().await.call_tool(req).await.unwrap();
		Ok(result)
	}
}

#[derive(Clone)]
pub struct ConnectionPool {
	state: Arc<std::sync::RwLock<XdsStore>>,

	by_name: Arc<RwLock<HashMap<String, Arc<RwLock<RunningService<ClientHandlerService>>>>>>,
}

impl ConnectionPool {
	pub fn new(state: Arc<std::sync::RwLock<XdsStore>>) -> Self {
		Self {
			state,
			by_name: Arc::new(RwLock::new(HashMap::new())),
		}
	}

	pub async fn get(&self, name: &str) -> Option<Arc<RwLock<RunningService<ClientHandlerService>>>> {
		tracing::info!("getting connection for target: {}", name);
		let by_name = self.by_name.read().await;
		match by_name.get(name) {
			Some(connection) => {
				tracing::info!("connection found for target: {}", name);
				Some(connection.clone())
			},
			None => {
				let target = { self.state.read().unwrap().targets.get(name).cloned() };
				match target {
					Some(target) => {
						// We want write access to the by_name map, so we drop the read lock
						// TODO: Fix this
						drop(by_name);
						let connection = self.connect(&target).await.unwrap();
						Some(connection)
					},
					None => {
						tracing::error!("Target not found: {}", name);
						// Need to demand it, but this should never happen
						None
					},
				}
			},
		}
	}

	pub async fn iter(
		&self,
	) -> impl Iterator<Item = (String, Arc<RwLock<RunningService<ClientHandlerService>>>)> {
		// Iterate through all state targets, and get the connection from the pool
		// If the connection is not in the pool, connect to it and add it to the pool
		tracing::info!("iterating over targets");
		let targets: Vec<(String, Target)> = {
			let state = self.state.read().unwrap();
			state
				.targets
				.iter()
				.map(|(name, target)| (name.clone(), target.clone()))
				.collect()
		};
		let x = targets.iter().map(|(name, target)| async move {
			let connection = self.get(name).await.unwrap();
			(name.clone(), connection)
		});

		let x = futures::future::join_all(x).await;
		tracing::info!("x: {:?}", x);
		x.into_iter()
	}

	async fn connect(
		&self,
		target: &Target,
	) -> Result<Arc<RwLock<RunningService<ClientHandlerService>>>, anyhow::Error> {
		tracing::info!("connecting to target: {}", target.name);
		let transport: RunningService<ClientHandlerService> = match &target.spec {
			TargetSpec::Sse { host, port } => {
				tracing::info!("starting sse transport for target: {}", target.name);
				let transport: SseTransport = SseTransport::start(
					format!("http://{}:{}", host, port).as_str(),
					Default::default(),
				)
				.await?;
				serve_client(ClientHandlerService::simple(), transport).await?
			},
			TargetSpec::Stdio { cmd, args } => {
				tracing::info!("starting stdio transport for target: {}", target.name);
				serve_client(
					ClientHandlerService::simple(),
					TokioChildProcess::new(Command::new(cmd).args(args)).unwrap(),
				)
				.await?
			},
		};
		let connection = Arc::new(RwLock::new(transport));
		tracing::info!("connection created for target: {}", target.name);
		// We need to drop this lock quick
		let mut by_name = self.by_name.write().await;
		by_name.insert(target.name.clone(), connection.clone());
		tracing::info!("connection inserted for target: {}", target.name);
		Ok(connection)
	}
}

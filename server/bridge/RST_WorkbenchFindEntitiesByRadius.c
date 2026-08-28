#ifdef WORKBENCH
class RST_WorkbenchFindEntitiesByRadiusRequest : JsonApiStruct
{
	float centerX;
	float centerY;
	float centerZ;
	float radiusMeters;
	string queryScope;
	bool requireObject;
	bool excludeProxies;
	string className;
	int limit;

	void RST_WorkbenchFindEntitiesByRadiusRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchFindEntitiesByRadiusResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	float centerX;
	float centerY;
	float centerZ;
	float radiusMeters;
	string queryScope;
	bool requireObject;
	bool excludeProxies;
	string entities;
	bool truncated;

	void RST_WorkbenchFindEntitiesByRadiusResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchRadiusCollector
{
	WorldEditorAPI m_Api;
	RST_WorkbenchFindEntitiesByRadiusRequest m_Request;
	RST_WorkbenchFindEntitiesByRadiusResponse m_Response;
	int m_Returned;

	void RST_WorkbenchRadiusCollector(WorldEditorAPI api, RST_WorkbenchFindEntitiesByRadiusRequest request, RST_WorkbenchFindEntitiesByRadiusResponse response)
	{
		m_Api = api;
		m_Request = request;
		m_Response = response;
	}

	bool AddEntity(IEntity entity)
	{
		IEntitySource source = m_Api.EntityToSource(entity);
		if (!source)
			return true;
		if (!m_Request.className.IsEmpty() && source.GetClassName().IndexOf(m_Request.className) == -1)
			return true;
		if (m_Returned >= m_Request.limit)
		{
			m_Response.truncated = true;
			return false;
		}
		if (!m_Response.entities.IsEmpty())
			m_Response.entities += ";";
		vector position = entity.GetOrigin();
		string resourceName = string.Format("%1", source.GetResourceName());
		string name = source.GetName();
		string subSceneName = entity.GetWorld().GetSubSceneName(source.GetSubScene());
		string layerName = m_Api.GetEntitySubsceneLayer(source.GetSubScene(), source);
		if (name == resourceName)
			name = string.Empty;
		resourceName.Replace("|", "/");
		resourceName.Replace(";", "/");
		name.Replace("|", "/");
		name.Replace(";", "/");
		subSceneName.Replace("|", "/");
		subSceneName.Replace(";", "/");
		layerName.Replace("|", "/");
		layerName.Replace(";", "/");
		m_Response.entities += string.Format("%1|%2|%3|%4|%5|%6|%7", source.GetID().ToString(), source.GetClassName(), source.GetSubScene(), source.GetLayerID(), position[0], position[1], position[2]) + "|" + resourceName + "|" + name + "|" + subSceneName + "|" + layerName;
		m_Returned++;
		return true;
	}
}

class RST_WorkbenchFindEntitiesByRadius : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchFindEntitiesByRadiusRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchFindEntitiesByRadiusRequest typedRequest = RST_WorkbenchFindEntitiesByRadiusRequest.Cast(request);
		RST_WorkbenchFindEntitiesByRadiusResponse response = new RST_WorkbenchFindEntitiesByRadiusResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.centerX = typedRequest.centerX;
		response.centerY = typedRequest.centerY;
		response.centerZ = typedRequest.centerZ;
		response.radiusMeters = typedRequest.radiusMeters;
		response.queryScope = typedRequest.queryScope;
		response.requireObject = typedRequest.requireObject;
		response.excludeProxies = typedRequest.excludeProxies;
		if (typedRequest.radiusMeters < 0.01 || typedRequest.radiusMeters > 50000 || typedRequest.limit < 1 || typedRequest.limit > 100)
		{
			response.status = "invalid-query";
			return response;
		}
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api || api.GetEditorEntityCount() < 1)
		{
			response.status = "world-editor-api-unavailable";
			return response;
		}
		IEntity root = api.SourceToEntity(api.GetEditorEntity(0));
		if (!root)
		{
			response.status = "world-unavailable";
			return response;
		}
		EQueryEntitiesFlags flags = EQueryEntitiesFlags.ALL;
		if (typedRequest.queryScope == "static")
			flags = EQueryEntitiesFlags.STATIC;
		else if (typedRequest.queryScope == "dynamic")
			flags = EQueryEntitiesFlags.DYNAMIC;
		else if (typedRequest.queryScope != "all")
		{
			response.status = "invalid-query-scope";
			return response;
		}
		if (typedRequest.requireObject)
			flags |= EQueryEntitiesFlags.WITH_OBJECT;
		if (typedRequest.excludeProxies)
			flags |= EQueryEntitiesFlags.NO_PROXIES;
		RST_WorkbenchRadiusCollector collector = new RST_WorkbenchRadiusCollector(api, typedRequest, response);
		root.GetWorld().QueryEntitiesBySphere(Vector(typedRequest.centerX, typedRequest.centerY, typedRequest.centerZ), typedRequest.radiusMeters, collector.AddEntity, null, flags);
		response.status = "available";
		return response;
	}
}
#endif

#ifdef WORKBENCH
class RST_WorkbenchEntityMutationRequest : JsonApiStruct
{
	string entityId;
	string parentEntityId;
	string resourceName;
	string name;
	int subScene;
	float x;
	float y;
	float z;
	float pitch;
	float yaw;
	float roll;
	float scale;
	int layerId;
	bool targetIsResource;
	bool confirm;

	void RST_WorkbenchEntityMutationRequest()
	{
		RegV("entityId");
		RegV("parentEntityId");
		RegV("resourceName");
		RegV("name");
		RegV("subScene");
		RegV("x");
		RegV("y");
		RegV("z");
		RegV("pitch");
		RegV("yaw");
		RegV("roll");
		RegV("scale");
		RegV("layerId");
		RegV("targetIsResource");
		RegV("confirm");
	}
}

class RST_WorkbenchEntityMutationResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	int activeLayerId;
	string entity;
	string destination;
	bool destinationExists;

	void RST_WorkbenchEntityMutationResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchEntityMutationBase : NetApiHandler
{
	IEntitySource Find(WorldEditorAPI api, string entityId)
	{
		IEntitySource candidate;
		int selectedCount = api.GetSelectedEntitiesCount();
		int editorCount = api.GetEditorEntityCount();
		for (int i; i < selectedCount; i++)
		{
			candidate = api.GetSelectedEntity(i);
			if (candidate && candidate.GetID().ToString() == entityId)
			{
				return candidate;
			}
		}
		for (int i; i < editorCount; i++)
		{
			candidate = api.GetEditorEntity(i);
			if (candidate && candidate.GetID().ToString() == entityId)
			{
				return candidate;
			}
		}
		return null;
	}

	bool IsAncestor(IEntitySource entity, IEntitySource candidateParent)
	{
		IEntitySource current = candidateParent;
		while (current)
		{
			if (current == entity)
				return true;
			current = current.GetParent();
		}
		return false;
	}

	bool Setup(WorldEditorAPI api, RST_WorkbenchEntityMutationResponse response)
	{
		if (!api)
		{
			response.status = "world-editor-api-unavailable";
			return false;
		}
		if (!api.GetWorld())
		{
			response.status = "world-unavailable";
			return false;
		}
		if (api.IsPrefabEditMode())
		{
			response.status = "prefab-edit-mode";
			return false;
		}
		if (api.IsDoingEditAction())
		{
			response.status = "editor-action-active";
			return false;
		}
		return true;
	}

	void Record(WorldEditorAPI api, RST_WorkbenchEntityMutationResponse response, IEntitySource entity)
	{
		IEntity runtimeEntity;
		vector p;
		string resourceName;
		string name;
		string subSceneName;
		string layerName;
		if (!entity)
			return;
		runtimeEntity = api.SourceToEntity(entity);
		if (runtimeEntity)
			p = runtimeEntity.GetOrigin();
		else
		{
			p = vector.Zero;
			entity.Get("coords", p);
		}
		resourceName = string.Format("%1", entity.GetResourceName());
		name = entity.GetName();
		subSceneName = api.GetWorld().GetSubSceneName(entity.GetSubScene());
		layerName = api.GetEntitySubsceneLayer(entity.GetSubScene(), entity);
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
		response.entity = string.Format(    "%1|%2|%3|%4|%5|%6|%7",    entity.GetID().ToString(),    entity.GetClassName(),    entity.GetSubScene(),    entity.GetLayerID(),    p[0],    p[1],    p[2]) + "|" + resourceName + "|" + name + "|" + subSceneName + "|" + layerName;
	}

	string PropertyTypeName(DataVarType dataType)
	{
		switch (dataType)
		{
			case DataVarType.BOOLEAN: return "bool";
			case DataVarType.INTEGER: return "integer";
			case DataVarType.SCALAR: return "float";
			case DataVarType.VECTOR3: return "vector";
			case DataVarType.STRING: return "string";
			case DataVarType.RESOURCE_NAME: return "resource";
		}
		return string.Empty;
	}

	bool ReadPropertyValue(BaseContainer container, string name, DataVarType dataType, out string value)
	{
		bool boolValue;
		int integerValue;
		float floatValue;
		vector vectorValue;
		string stringValue;
		switch (dataType)
		{
			case DataVarType.BOOLEAN:     if (!container.Get(name, boolValue)) return false;
			if (boolValue)
				value = "1";
			else
				value = "0";
			return true;
			case DataVarType.INTEGER:     if (!container.Get(name, integerValue)) return false;
			value = integerValue.ToString();
			return true;
			case DataVarType.SCALAR:     if (!container.Get(name, floatValue)) return false;
			value = floatValue.ToString();
			return true;
			case DataVarType.VECTOR3:     if (!container.Get(name, vectorValue)) return false;
			value = string.Format("%1 %2 %3", vectorValue[0], vectorValue[1], vectorValue[2]);
			return true;
			case DataVarType.STRING:    case DataVarType.RESOURCE_NAME:     if (!container.Get(name, stringValue)) return false;
			value = stringValue;
			return true;
		}
		return false;
	}

	RST_WorkbenchEntityMutationResponse Response()
	{
		RST_WorkbenchEntityMutationResponse response = new RST_WorkbenchEntityMutationResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.activeLayerId = -1;
		return response;
	}
}

class RST_WorkbenchCreateEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		response.activeLayerId = api.GetCurrentEntityLayerId();
		if (r.resourceName.IsEmpty() || r.subScene < 0 || r.layerId < 0    || api.IsEntityLayerLockedHierarchy(api.GetCurrentSubScene(), r.layerId))
		{
			response.status = "invalid-create-target";
			return response;
		}
		if (r.targetIsResource)
		{
			ResourceName prefab = r.resourceName;
			Resource resource = Resource.Load(prefab);
			if (!resource || !resource.IsValid())
			{
				response.status = "resource-load-failed";
				return response;
			}
		}
		if (!api.BeginEntityAction("Reforger Script Tools: create entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		IEntitySource entity = api.CreateEntity(    r.resourceName,    r.name,    r.layerId,    null,    Vector(r.x, r.y, r.z),    Vector(r.pitch, r.yaw, r.roll));
		api.EndEntityAction("Reforger Script Tools: create entity");
		if (!entity)
		{
			response.status = "create-rejected";
			return response;
		}
		Record(api, response, entity);
		response.activeLayerId = entity.GetLayerID();
		if (entity.GetSubScene() != r.subScene || entity.GetLayerID() != r.layerId)
			response.status = "target-mismatch";
		else
			response.status = "created";
		return response;
	}
}

class RST_WorkbenchRenameEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID())    || !api.BeginEntityAction("Reforger Script Tools: rename entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		bool changed = api.RenameEntity(entity, r.name);
		api.EndEntityAction("Reforger Script Tools: rename entity");
		if (!changed)
		{
			response.status = "mutation-rejected";
			return response;
		}
		Record(api, response, entity);
		response.status = "renamed";
		return response;
	}
}

class RST_WorkbenchMoveEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID())    || !api.BeginEntityAction("Reforger Script Tools: move entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!api.SetVariableValue(entity, null, "coords", string.Format("%1 %2 %3", r.x, r.y, r.z)))
		{
			api.EndEntityAction("Reforger Script Tools: move entity");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: move entity");
		Record(api, response, entity);
		response.status = "moved";
		return response;
	}
}

class RST_WorkbenchRotateEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID()))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!api.BeginEntityAction("Reforger Script Tools: rotate entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		bool changed = api.SetVariableValue(    entity,    null,    "angles",    string.Format("%1 %2 %3", r.pitch, r.yaw, r.roll));
		api.EndEntityAction("Reforger Script Tools: rotate entity");
		if (!changed)
		{
			response.status = "mutation-rejected";
			return response;
		}
		Record(api, response, entity);
		response.status = "rotated";
		return response;
	}
}

class RST_WorkbenchReparentEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		IEntitySource parent = Find(api, r.parentEntityId);
		if (!parent)
		{
			response.status = "parent-entity-not-found";
			return response;
		}
		if (entity == parent || entity.GetSubScene() != parent.GetSubScene()    || IsAncestor(entity, parent)    || api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID())    || api.IsEntityLayerLockedHierarchy(parent.GetSubScene(), parent.GetLayerID())    || !api.BeginEntityAction("Reforger Script Tools: reparent entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		bool changed = api.ParentEntity(parent, entity, true);
		api.EndEntityAction("Reforger Script Tools: reparent entity");
		if (!changed)
		{
			response.status = "mutation-rejected";
			return response;
		}
		Record(api, response, entity);
		response.status = "reparented";
		return response;
	}
}

class RST_WorkbenchDuplicateEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID())    || !api.BeginEntityAction("Reforger Script Tools: duplicate entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		IEntitySource clone = api.CreateClonedEntity(entity, r.name, null, false);
		bool moved = clone && api.SetVariableValue(    clone,    null,    "coords",    string.Format("%1 %2 %3", r.x, r.y, r.z));
		api.EndEntityAction("Reforger Script Tools: duplicate entity");
		if (!clone || !moved)
		{
			response.status = "mutation-rejected";
			return response;
		}
		Record(api, response, clone);
		response.status = "duplicated";
		return response;
	}
}

class RST_WorkbenchDeleteEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		Record(api, response, entity);
		if (!r.confirm)
		{
			response.status = "confirmation-required";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID())    || !api.BeginEntityAction("Reforger Script Tools: delete entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		bool deleted = api.DeleteEntity(entity);
		api.EndEntityAction("Reforger Script Tools: delete entity");
		response.entity = string.Empty;
		if (deleted && !Find(api, r.entityId))
			response.status = "deleted";
		else
			response.status = "mutation-rejected";
		return response;
	}
}

class RST_WorkbenchEntityTransformResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;
	string position;
	string angles;
	float scale;

	void RST_WorkbenchEntityTransformResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchTransformEntity : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchEntityMutationRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchEntityMutationRequest r = RST_WorkbenchEntityMutationRequest.Cast(request);
		RST_WorkbenchEntityTransformResponse response = new RST_WorkbenchEntityTransformResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.scale = 0;
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		RST_WorkbenchEntityMutationResponse setupResponse = Response();
		if (!Setup(api, setupResponse))
		{
			response.status = setupResponse.status;
			return response;
		}
		IEntitySource entity = Find(api, r.entityId);
		if (!entity)
		{
			response.status = "entity-not-found";
			return response;
		}
		if (api.IsEntityLayerLockedHierarchy(entity.GetSubScene(), entity.GetLayerID()) || !api.BeginEntityAction("Reforger Script Tools: transform entity"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		bool changed = api.SetVariableValue(entity, null, "coords", string.Format("%1 %2 %3", r.x, r.y, r.z));
		changed = api.SetVariableValue(entity, null, "angles", string.Format("%1 %2 %3", r.pitch, r.yaw, r.roll)) && changed;
		changed = api.SetVariableValue(entity, null, "scale", r.scale.ToString()) && changed;
		api.EndEntityAction("Reforger Script Tools: transform entity");
		if (!changed)
		{
			response.status = "mutation-rejected";
			return response;
		}
		IEntity runtimeEntity = api.SourceToEntity(entity);
		if (!runtimeEntity)
		{
			response.status = "readback-unavailable";
			return response;
		}
		vector position = runtimeEntity.GetOrigin();
		vector angles = runtimeEntity.GetAngles();
		response.entity = entity.GetID().ToString() + "|" + entity.GetClassName() + "|" + entity.GetSubScene().ToString() + "|" + entity.GetLayerID().ToString() + "|" + position[0].ToString() + "|" + position[1].ToString() + "|" + position[2].ToString();
		response.position = string.Format("%1 %2 %3", position[0], position[1], position[2]);
		response.angles = string.Format("%1 %2 %3", angles[0], angles[1], angles[2]);
		response.scale = runtimeEntity.GetScale();
		response.status = "transformed";
		return response;
	}
}
#endif

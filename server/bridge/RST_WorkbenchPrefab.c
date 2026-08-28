#ifdef WORKBENCH
class RST_WorkbenchPrefabRequest : JsonApiStruct
{
	string entityId;
	string resourceName;
	string memberId;

	void RST_WorkbenchPrefabRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchPrefabResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;
	string memberId;
	string resourceName;
	string resourceReferenceKind;
	string contributorAddons;
	string ancestorResources;
	bool ancestorResourcesTruncated;
	bool prefabEditMode;
	string components;
	string componentProperties;
	string children;
	bool childrenTruncated;
	string properties;
	bool propertiesTruncated;
	int childCount;

	void RST_WorkbenchPrefabResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchInspectPrefab : NetApiHandler
{
	IEntitySource Find(WorldEditorAPI api, string id)
	{
		for (int i, count = api.GetEditorEntityCount(); i < count; i++)
		{
			IEntitySource candidate = api.GetEditorEntity(i);
			if (candidate && candidate.GetID().ToString() == id)
				return candidate;
		}
		return null;
	}

	// Keep resource and entity inspection on the same authored-component path.
	int ComponentCount(BaseContainer source)
	{
		IEntitySource entity = IEntitySource.Cast(source);
		int count;
		ref BaseContainerList components;
		if (entity)
		{
			count = entity.GetComponentCount();
			if (count != 0)
				return count;
		}
		components = source.GetObjectArray("components");
		if (components)
			return components.Count();
		return 0;
	}

	IEntityComponentSource ComponentAt(BaseContainer source, int index)
	{
		IEntitySource entity = IEntitySource.Cast(source);
		ref BaseContainerList components;
		if (entity && entity.GetComponentCount() > 0)
			return entity.GetComponent(index);
		components = source.GetObjectArray("components");
		if (components)
			return IEntityComponentSource.Cast(components.Get(index));
		return null;
	}

	BaseContainer FindMember(BaseContainer parent, string memberId, string prefix, int depth)
	{
		if (depth >= 16)
			return null;
		int firstChildIndex;
		if (parent.GetNumChildren() > 0 && !parent.GetChild(0) && parent.GetChild(1))
			firstChildIndex = 1;
		for (int index, count = parent.GetNumChildren(); index < count; index++)
		{
			BaseContainer child = parent.GetChild(index + firstChildIndex);
			if (!child)
				continue;
			string childId = string.Format("member:%1", index);
			if (!prefix.IsEmpty())
				childId = string.Format("%1/%2", prefix, childId);
			if (memberId == childId)
				return child;
			BaseContainer result = FindMember(child, memberId, childId, depth + 1);
			if (result)
				return result;
		}
		return null;
	}

	BaseContainer ResolveMember(IEntitySource root, string memberId)
	{
		if (memberId.IsEmpty())
			return root;
		return FindMember(root, memberId, string.Empty, 0);
	}

	bool IsEngineCallback(string name)
	{
		return name.StartsWith("EOn") || name.StartsWith("_WB_") || name == "RplLoad"    || name == "RplSave" || name == "Preload" || name == "OnTransformResetImpl"    || name == "userScript" || name == "constructor" || name == "destructor";
	}

	string PropertyOrigin(BaseContainer container, string name)
	{
		if (container.IsVariableSetDirectly(name))
			return "direct";
		for (BaseContainer ancestor = container.GetAncestor(); ancestor; ancestor = ancestor.GetAncestor())
		{
			if (ancestor.IsVariableSetDirectly(name))
				return "inherited";
		}
		return "default";
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

	string PropertyRecord(int componentIndex, BaseContainer container, string name, string typeName, string value)
	{
		name.Replace("|", "/");
		name.Replace(";", "/");
		value.Replace("|", "/");
		value.Replace(";", "/");
		string origin = PropertyOrigin(container, name);
		if (componentIndex >= 0)
			return string.Format("%1|%2|%3|%4|%5|%6", componentIndex, name, typeName, value, container.IsVariableSetDirectly(name), origin);
		return string.Format("%1|%2|%3|%4|%5", name, typeName, value, container.IsVariableSetDirectly(name), origin);
	}

	bool ReadPropertyRecord(int componentIndex, BaseContainer container, string name, DataVarType dataType, out string record)
	{
		bool boolValue;
		int integerValue;
		float floatValue;
		vector vectorValue;
		string stringValue;
		ResourceName resourceValue;
		string typeName = PropertyTypeName(dataType);
		if (typeName.IsEmpty())
			return false;
		switch (dataType)
		{
			case DataVarType.BOOLEAN:     if (!container.Get(name, boolValue))      return false;
			record = PropertyRecord(componentIndex, container, name, typeName, boolValue.ToString());
			return true;
			case DataVarType.INTEGER:     if (!container.Get(name, integerValue))      return false;
			record = PropertyRecord(componentIndex, container, name, typeName, integerValue.ToString());
			return true;
			case DataVarType.SCALAR:     if (!container.Get(name, floatValue))      return false;
			record = PropertyRecord(componentIndex, container, name, typeName, floatValue.ToString());
			return true;
			case DataVarType.VECTOR3:     if (!container.Get(name, vectorValue))      return false;
			record = PropertyRecord(componentIndex, container, name, typeName, vectorValue.ToString(false));
			return true;
			case DataVarType.STRING:     if (!container.Get(name, stringValue))      return false;
			record = PropertyRecord(componentIndex, container, name, typeName, stringValue);
			return true;
			case DataVarType.RESOURCE_NAME:     if (!container.Get(name, resourceValue))      return false;
			record = PropertyRecord(componentIndex, container, name, typeName, resourceValue);
			return true;
		}
		return false;
	}

	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPrefabRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPrefabRequest r = RST_WorkbenchPrefabRequest.Cast(request);
		RST_WorkbenchPrefabResponse response = new RST_WorkbenchPrefabResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		// Keep the resource handle alive while any source/container derived from it is read.
		ref Resource resource;
		IEntitySource source;
		if (!r.entityId.IsEmpty())
		{
			source = Find(api, r.entityId);
		}
		else if (!r.resourceName.IsEmpty())
		{
			ResourceName prefabName = r.resourceName;
			resource = Resource.Load(prefabName);
			if (resource && resource.IsValid())
				source = resource.GetResource().ToEntitySource();
		}
		if (!source)
		{
			response.status = "prefab-not-found";
			return response;
		}
		BaseContainer target = ResolveMember(source, r.memberId);
		if (!target)
		{
			response.status = "member-not-found";
			return response;
		}
		response.memberId = r.memberId;
		if (!r.entityId.IsEmpty())
			response.entity = string.Format("%1|%2|%3|%4", source.GetID().ToString(), source.GetClassName(), source.GetSubScene(), source.GetLayerID());
		ResourceName resourceName = source.GetResourceName();
		response.resourceName = resourceName;
		if (resourceName.IsExternal())
			response.resourceReferenceKind = "external";
		else if (resourceName.IsInternal())
			response.resourceReferenceKind = "internal";
		else
			response.resourceReferenceKind = "path";
		array<string> addons = {
		};
		source.GetSourceAddons(addons);
		foreach (string addon : addons)
		{
			addon.Replace(";", "/");
			if (!response.contributorAddons.IsEmpty())
				response.contributorAddons += ";";
			response.contributorAddons += addon;
		}
		BaseContainer ancestor = source.GetAncestor();
		for (int depth = 0; ancestor && depth < 16; depth++)
		{
			ResourceName ancestorName = ancestor.GetResourceName();
			string ancestorResource = string.Format("%1", ancestorName);
			ancestorResource.Replace(";", "/");
			if (!response.ancestorResources.IsEmpty())
				response.ancestorResources += ";";
			response.ancestorResources += ancestorResource;
			ancestor = ancestor.GetAncestor();
		}
		if (ancestor)
			response.ancestorResourcesTruncated = true;
		response.prefabEditMode = api.IsPrefabEditMode();
		response.childCount = target.GetNumChildren();
		for (int i, count = ComponentCount(target); i < count && i < 64; i++)
		{
			IEntityComponentSource component = ComponentAt(target, i);
			if (!component)
				continue;
			if (!response.components.IsEmpty())
				response.components += ";";
			response.components += string.Format("%1|%2", i, component.GetClassName());
			for (int propertyIndex, propertyCount = component.GetNumVars(); propertyIndex < propertyCount; propertyIndex++)
			{
				string name = component.GetVarName(propertyIndex);
				DataVarType dataType = component.GetDataVarType(propertyIndex);
				string record;
				if (IsEngineCallback(name))
					continue;
				if (!ReadPropertyRecord(i, component, name, dataType, record))
					continue;
				if (!response.componentProperties.IsEmpty())
					response.componentProperties += ";";
				response.componentProperties += record;
			}
		}
		int firstChildIndex;
		if (target.GetNumChildren() > 0 && !target.GetChild(0) && target.GetChild(1))
			firstChildIndex = 1;
		for (int i, count = target.GetNumChildren(), returned; i < count; i++)
		{
			BaseContainer child = target.GetChild(i + firstChildIndex);
			if (!child)
				continue;
			if (returned >= 64)
			{
				response.childrenTruncated = true;
				break;
			}
			string className = child.GetClassName();
			string name = child.GetName();
			className.Replace("|", "/");
			className.Replace(";", "/");
			name.Replace("|", "/");
			name.Replace(";", "/");
			if (!response.children.IsEmpty())
				response.children += ";";
			response.children += string.Format("%1|%2|%3", i, className, name);
			returned++;
		}
		for (int i, count = target.GetNumVars(), returned; i < count; i++)
		{
			string name = target.GetVarName(i);
			DataVarType dataType;
			string record;
			if (IsEngineCallback(name))
				continue;
			if (!target.IsVariableSetDirectly(name) && name != "coords" && name != "angles" && name != "scale")
				continue;
			if (returned >= 128)
			{
				response.propertiesTruncated = true;
				break;
			}
			dataType = target.GetDataVarType(i);
			name.Replace("|", "/");
			name.Replace(";", "/");
			if (!ReadPropertyRecord(-1, target, name, dataType, record))
				continue;
			if (!response.properties.IsEmpty())
				response.properties += ";";
			response.properties += record;
			returned++;
		}
		response.status = "available";
		return response;
	}
}

class RST_WorkbenchCreatePrefab : RST_WorkbenchEntityMutationBase
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
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		IEntitySource entity = Find(api, r.entityId);
		if (!entity || r.name.IsEmpty() || !r.name.EndsWith(".et") || r.name.Contains("..") || r.name.Contains(":") || r.name.StartsWith("/") || r.name.StartsWith("\\"))
		{
			response.status = "invalid-prefab-destination";
			return response;
		}
		string absolutePath;
		if (!Workbench.GetAbsolutePath(r.name, absolutePath, false))
		{
			response.status = "invalid-prefab-destination";
			return response;
		}
		response.destination = r.name;
		response.destinationExists = FileIO.FileExists(absolutePath);
		if (response.destinationExists)
		{
			response.status = "prefab-destination-exists";
			return response;
		}
		if (!r.confirm)
		{
			Record(api, response, entity);
			response.status = "confirmation-required";
			return response;
		}
		if (!api.CreateEntityTemplate(entity, absolutePath))
		{
			response.status = "prefab-create-rejected";
			return response;
		}
		Record(api, response, entity);
		response.status = "prefab-created";
		return response;
	}
}

class RST_WorkbenchCreateGenericPrefab : RST_WorkbenchEntityMutationBase
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
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		int subScene = api.GetCurrentSubScene();
		int layerId = api.GetCurrentEntityLayerId();
		response.activeLayerId = layerId;
		if (r.name.IsEmpty() || !r.name.EndsWith(".et") || r.name.Contains("..")    || r.name.Contains(":") || r.name.StartsWith("/") || r.name.StartsWith("\\"))
		{
			response.status = "invalid-prefab-destination";
			return response;
		}
		string absolutePath;
		if (!Workbench.GetAbsolutePath(r.name, absolutePath, false))
		{
			response.status = "invalid-prefab-destination";
			return response;
		}
		response.destination = r.name;
		response.destinationExists = FileIO.FileExists(absolutePath);
		if (response.destinationExists)
		{
			response.status = "prefab-destination-exists";
			return response;
		}
		if (subScene < 0 || layerId < 0 || api.IsEntityLayerLockedHierarchy(subScene, layerId))
		{
			response.status = "invalid-create-target";
			return response;
		}
		if (!r.confirm)
		{
			response.status = "confirmation-required";
			return response;
		}
		if (!api.BeginEntityAction("Reforger Script Tools: create GenericEntity prefab"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		IEntitySource temporary = api.CreateEntity(    "GenericEntity",    "RST_PrefabTemplateSource",    layerId,    null,    Vector(0, 0, 0),    Vector(0, 0, 0));
		if (!temporary)
		{
			api.EndEntityAction("Reforger Script Tools: create GenericEntity prefab");
			response.status = "generic-entity-create-rejected";
			return response;
		}
		bool templateCreated = api.CreateEntityTemplate(temporary, absolutePath);
		bool temporaryDeleted = api.DeleteEntity(temporary);
		api.EndEntityAction("Reforger Script Tools: create GenericEntity prefab");
		if (!templateCreated)
		{
			response.status = "prefab-create-rejected";
			return response;
		}
		if (!temporaryDeleted)
		{
			response.status = "temporary-entity-cleanup-failed";
			return response;
		}
		response.status = "generic-prefab-created";
		return response;
	}
}

class RST_WorkbenchSavePrefab : RST_WorkbenchEntityMutationBase
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
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		IEntitySource entity = Find(api, r.entityId);
		ref Resource resource;
		if (r.entityId.IsEmpty())
		{
			if (r.resourceName.IsEmpty())
			{
				response.status = "invalid-prefab-target";
				return response;
			}
			ResourceName resourceName = r.resourceName;
			resource = Resource.Load(resourceName);
			if (!resource || !resource.IsValid() || !resource.GetResource())
			{
				response.status = "prefab-not-found";
				return response;
			}
			entity = IEntitySource.Cast(resource.GetResource().ToBaseContainer());
			if (!entity)
			{
				response.status = "prefab-source-unavailable";
				return response;
			}
		}
		else if (!entity || !api.IsPrefabEditMode())
		{
			response.status = "prefab-edit-unavailable";
			return response;
		}
		if (!r.confirm)
		{
			if (!r.entityId.IsEmpty())
				Record(api, response, entity);
			response.status = "confirmation-required";
			return response;
		}
		if (!api.SaveEntityTemplate(entity))
		{
			response.status = "prefab-save-rejected";
			return response;
		}
		if (!r.entityId.IsEmpty())
			Record(api, response, entity);
		response.status = "prefab-saved";
		return response;
	}
}
// This is a bounded acceptance gate, not a general-purpose mutation endpoint.
// It proves that Workbench can mutate and save a resource-loaded IEntitySource
// without using a scene instance or a resource-file writer. The component is
// created and removed in the same native editor action, so a successful save
// leaves the resource's effective component count unchanged.

class RST_WorkbenchPrefabResourceProofRequest : JsonApiStruct
{
	string resourceName;
	bool confirm;

	void RST_WorkbenchPrefabResourceProofRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchPrefabResourceProofResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string resourceName;
	bool resourceLoaded;
	bool sourceResolved;
	bool actionBegun;
	bool componentCreated;
	bool componentDeleted;
	bool templateSaved;
	int componentCountBefore;
	int componentCountAfterReload;

	void RST_WorkbenchPrefabResourceProofResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchPrefabResourceProof : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPrefabResourceProofRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPrefabResourceProofRequest r = RST_WorkbenchPrefabResourceProofRequest.Cast(request);
		RST_WorkbenchPrefabResourceProofResponse response = new RST_WorkbenchPrefabResourceProofResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.resourceName = r.resourceName;
		if (r.resourceName.IsEmpty())
		{
			response.status = "invalid-prefab-target";
			return response;
		}
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		ResourceName prefabName = r.resourceName;
		ref Resource resource = Resource.Load(prefabName);
		if (!resource || !resource.IsValid() || !resource.GetResource())
		{
			response.status = "resource-load-failed";
			return response;
		}
		response.resourceLoaded = true;
		IEntitySource source = IEntitySource.Cast(resource.GetResource().ToBaseContainer());
		if (!source)
		{
			response.status = "prefab-source-unavailable";
			return response;
		}
		response.sourceResolved = true;
		response.componentCountBefore = source.GetComponentCount();
		if (!r.confirm)
		{
			response.status = "confirmation-required";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api.BeginEntityAction("Reforger Script Tools: resource prefab proof"))
		{
			response.status = "action-rejected";
			return response;
		}
		response.actionBegun = true;
		IEntityComponentSource component = api.CreateComponent(source, "ScriptComponent");
		if (!component)
		{
			api.EndEntityAction("Reforger Script Tools: resource prefab proof");
			response.status = "component-create-rejected";
			return response;
		}
		response.componentCreated = true;
		if (!api.DeleteComponent(source, component))
		{
			api.EndEntityAction("Reforger Script Tools: resource prefab proof");
			response.status = "component-delete-rejected";
			return response;
		}
		response.componentDeleted = true;
		api.EndEntityAction("Reforger Script Tools: resource prefab proof");
		if (!api.SaveEntityTemplate(source))
		{
			response.status = "prefab-save-rejected";
			return response;
		}
		response.templateSaved = true;
		resource = null;
		resource = Resource.Load(prefabName);
		if (!resource || !resource.IsValid() || !resource.GetResource())
		{
			response.status = "reload-failed";
			return response;
		}
		IEntitySource reloadedSource = IEntitySource.Cast(resource.GetResource().ToBaseContainer());
		if (!reloadedSource)
		{
			response.status = "reload-source-unavailable";
			return response;
		}
		response.componentCountAfterReload = reloadedSource.GetComponentCount();
		if (response.componentCountAfterReload != response.componentCountBefore)
		{
			response.status = "reload-mismatch";
			return response;
		}
		response.status = "proof-succeeded";
		return response;
	}
}

class RST_WorkbenchPrefabResourceComponentRequest : JsonApiStruct
{
	string resourceName;
	string className;
	bool confirm;

	void RST_WorkbenchPrefabResourceComponentRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchPrefabResourceComponentResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string resourceName;
	int componentIndex;
	string componentClass;
	bool templateSaved;

	void RST_WorkbenchPrefabResourceComponentResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchAddPrefabResourceComponent : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPrefabResourceComponentRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPrefabResourceComponentRequest r = RST_WorkbenchPrefabResourceComponentRequest.Cast(request);
		RST_WorkbenchPrefabResourceComponentResponse response = new RST_WorkbenchPrefabResourceComponentResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.resourceName = r.resourceName;
		response.componentIndex = -1;
		if (r.resourceName.IsEmpty() || r.className.IsEmpty())
		{
			response.status = "invalid-component-request";
			return response;
		}
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		ResourceName prefabName = r.resourceName;
		ref Resource resource = Resource.Load(prefabName);
		if (!resource || !resource.IsValid() || !resource.GetResource())
		{
			response.status = "resource-load-failed";
			return response;
		}
		IEntitySource source = IEntitySource.Cast(resource.GetResource().ToBaseContainer());
		if (!source)
		{
			response.status = "prefab-source-unavailable";
			return response;
		}
		if (!r.confirm)
		{
			response.status = "confirmation-required";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api.BeginEntityAction("Reforger Script Tools: add prefab resource component"))
		{
			response.status = "action-rejected";
			return response;
		}
		IEntityComponentSource component = api.CreateComponent(source, r.className);
		if (!component)
		{
			api.EndEntityAction("Reforger Script Tools: add prefab resource component");
			response.status = "component-create-rejected";
			return response;
		}
		response.componentClass = component.GetClassName();
		for (int index, count = source.GetComponentCount(); index < count; index++)
		{
			if (source.GetComponent(index) == component)
			{
				response.componentIndex = index;
				break;
			}
		}
		api.EndEntityAction("Reforger Script Tools: add prefab resource component");
		if (!api.SaveEntityTemplate(source))
		{
			response.status = "prefab-save-rejected";
			return response;
		}
		response.templateSaved = true;
		response.status = "prefab-component-added";
		return response;
	}
}

class RST_WorkbenchPrefabResourceComponentRemovalRequest : JsonApiStruct
{
	string resourceName;
	string className;
	int componentIndex;
	bool confirm;

	void RST_WorkbenchPrefabResourceComponentRemovalRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchRemovePrefabResourceComponent : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPrefabResourceComponentRemovalRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPrefabResourceComponentRemovalRequest r = RST_WorkbenchPrefabResourceComponentRemovalRequest.Cast(request);
		RST_WorkbenchPrefabResourceComponentResponse response = new RST_WorkbenchPrefabResourceComponentResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.resourceName = r.resourceName;
		response.componentIndex = r.componentIndex;
		response.componentClass = r.className;
		if (r.resourceName.IsEmpty() || r.className.IsEmpty() || r.componentIndex < 0)
		{
			response.status = "invalid-component-request";
			return response;
		}
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		ResourceName prefabName = r.resourceName;
		ref Resource resource = Resource.Load(prefabName);
		if (!resource || !resource.IsValid() || !resource.GetResource())
		{
			response.status = "resource-load-failed";
			return response;
		}
		IEntitySource source = IEntitySource.Cast(resource.GetResource().ToBaseContainer());
		if (!source)
		{
			response.status = "prefab-source-unavailable";
			return response;
		}
		if (r.componentIndex >= source.GetComponentCount())
		{
			response.status = "stale-component-observation";
			return response;
		}
		IEntityComponentSource component = source.GetComponent(r.componentIndex);
		if (!component || component.GetClassName() != r.className)
		{
			response.status = "stale-component-observation";
			return response;
		}
		if (!r.confirm)
		{
			response.status = "confirmation-required";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api.BeginEntityAction("Reforger Script Tools: remove prefab resource component"))
		{
			response.status = "action-rejected";
			return response;
		}
		if (!api.DeleteComponent(source, component))
		{
			api.EndEntityAction("Reforger Script Tools: remove prefab resource component");
			response.status = "component-delete-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: remove prefab resource component");
		if (!api.SaveEntityTemplate(source))
		{
			response.status = "prefab-save-rejected";
			return response;
		}
		response.templateSaved = true;
		response.status = "prefab-component-removed";
		return response;
	}
}

class RST_WorkbenchPrefabResourcePropertyRequest : JsonApiStruct
{
	string resourceName;
	int componentIndex;
	string componentClass;
	string propertyName;
	string expectedValue;
	string value;
	bool confirm;

	void RST_WorkbenchPrefabResourcePropertyRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchPrefabResourcePropertyResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string resourceName;
	bool templateSaved;

	void RST_WorkbenchPrefabResourcePropertyResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchSetPrefabResourceProperty : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPrefabResourcePropertyRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPrefabResourcePropertyRequest r = RST_WorkbenchPrefabResourcePropertyRequest.Cast(request);
		RST_WorkbenchPrefabResourcePropertyResponse response = new RST_WorkbenchPrefabResourcePropertyResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.resourceName = r.resourceName;
		if (r.resourceName.IsEmpty() || r.propertyName.IsEmpty())
		{
			response.status = "invalid-property-request";
			return response;
		}
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		ResourceName prefabName = r.resourceName;
		ref Resource resource = Resource.Load(prefabName);
		if (!resource || !resource.IsValid() || !resource.GetResource())
		{
			response.status = "resource-load-failed";
			return response;
		}
		IEntitySource source = IEntitySource.Cast(resource.GetResource().ToBaseContainer());
		if (!source)
		{
			response.status = "prefab-source-unavailable";
			return response;
		}
		BaseContainer target = source;
		if (r.componentIndex >= 0)
		{
			if (r.componentIndex >= source.GetComponentCount())
			{
				response.status = "stale-component-observation";
				return response;
			}
			IEntityComponentSource component = source.GetComponent(r.componentIndex);
			if (!component || component.GetClassName() != r.componentClass)
			{
				response.status = "stale-component-observation";
				return response;
			}
			target = component;
		}
		int propertyIndex = target.GetVarIndex(r.propertyName);
		string observedValue;
		if (propertyIndex < 0    || !ReadPropertyValue(target, r.propertyName, target.GetDataVarType(propertyIndex), observedValue)    || observedValue != r.expectedValue)
		{
			response.status = "stale-property-observation";
			return response;
		}
		if (!r.confirm)
		{
			response.status = "confirmation-required";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api.BeginEntityAction("Reforger Script Tools: set prefab resource property"))
		{
			response.status = "action-rejected";
			return response;
		}
		array<ref ContainerIdPathEntry> path;
		if (r.componentIndex >= 0)
			path = {
			new ContainerIdPathEntry("components", r.componentIndex)
		};
		if (!api.SetVariableValue(source, path, r.propertyName, r.value))
		{
			api.EndEntityAction("Reforger Script Tools: set prefab resource property");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: set prefab resource property");
		if (!api.SaveEntityTemplate(source))
		{
			response.status = "prefab-save-rejected";
			return response;
		}
		response.templateSaved = true;
		response.status = "prefab-property-set";
		return response;
	}
}

class RST_WorkbenchPrefabPropertyRequest : JsonApiStruct
{
	string entityId;
	string propertyName;
	string expectedValue;
	string value;

	void RST_WorkbenchPrefabPropertyRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchSetPrefabProperty : RST_WorkbenchEntityMutationBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchPrefabPropertyRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchPrefabPropertyRequest r = RST_WorkbenchPrefabPropertyRequest.Cast(request);
		RST_WorkbenchEntityMutationResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor || !editor.GetApi())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		IEntitySource entity = Find(api, r.entityId);
		if (!entity || !api.IsPrefabEditMode() || r.propertyName.IsEmpty())
		{
			response.status = "prefab-edit-unavailable";
			return response;
		}
		int index = entity.GetVarIndex(r.propertyName);
		string observed;
		if (index < 0 || !ReadPropertyValue(entity, r.propertyName, entity.GetDataVarType(index), observed) || observed != r.expectedValue)
		{
			response.status = "stale-property-observation";
			return response;
		}
		if (!api.BeginEntityAction("Reforger Script Tools: set prefab property") || !api.SetVariableValue(entity, null, r.propertyName, r.value))
		{
			api.EndEntityAction("Reforger Script Tools: set prefab property");
			response.status = "mutation-rejected";
			return response;
		}
		api.EndEntityAction("Reforger Script Tools: set prefab property");
		Record(api, response, entity);
		response.status = "prefab-property-set";
		return response;
	}
}
#endif

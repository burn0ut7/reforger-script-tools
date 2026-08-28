#ifdef WORKBENCH
class RST_WorkbenchInspectEntityRequest : JsonApiStruct
{
	string entityId;

	void RST_WorkbenchInspectEntityRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchInspectEntityResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	bool editorAvailable;
	string status;
	string entity;
	string resourceName;
	string resourceReferenceKind;
	string contributorAddons;
	bool contributorAddonsTruncated;
	string ancestors;
	bool ancestorsTruncated;
	string children;
	bool childrenTruncated;
	string components;
	string componentProperties;
	bool componentPropertiesTruncated;
	string properties;
	bool propertiesTruncated;

	void RST_WorkbenchInspectEntityResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchInspectEntity : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchInspectEntityRequest();
	}

	protected void AppendEntity(out string records, WorldEditorAPI api, IEntitySource entity)
	{
		if (!entity)
			return;
		if (!records.IsEmpty())
			records += ";";
		IEntity runtimeEntity = api.SourceToEntity(entity);
		if (!runtimeEntity)
		{
			records += string.Format("%1|%2|%3|%4", entity.GetID().ToString(), entity.GetClassName(), entity.GetSubScene(), entity.GetLayerID());
			return;
		}
		vector transform[4];
		runtimeEntity.GetTransform(transform);
		string resourceName = string.Format("%1", entity.GetResourceName());
		string name = entity.GetName();
		string subSceneName = runtimeEntity.GetWorld().GetSubSceneName(entity.GetSubScene());
		string layerName = api.GetEntitySubsceneLayer(entity.GetSubScene(), entity);
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
		records += string.Format("%1|%2|%3|%4|%5|%6|%7", entity.GetID().ToString(), entity.GetClassName(), entity.GetSubScene(), entity.GetLayerID(), transform[3][0], transform[3][1], transform[3][2]) + "|" + resourceName + "|" + name + "|" + subSceneName + "|" + layerName;
	}

	// Workbench exposes authored components through either its direct component
	// list or the `components` container, depending on the editor context.
	int ComponentCount(IEntitySource entity)
	{
		int count = entity.GetComponentCount();
		if (count == 0)
		{
			ref BaseContainerList components = entity.GetObjectArray("components");
			if (components)
				count = components.Count();
		}
		return count;
	}

	IEntityComponentSource ComponentAt(IEntitySource entity, int index)
	{
		IEntityComponentSource component;
		int count = entity.GetComponentCount();
		if (count > 0)
			component = entity.GetComponent(index);
		else
		{
			ref BaseContainerList components = entity.GetObjectArray("components");
			if (components)
				component = IEntityComponentSource.Cast(components.Get(index));
		}
		return component;
	}

	protected bool IsEngineCallback(string name)
	{
		return name.StartsWith("EOn") || name.StartsWith("_WB_") || name == "RplLoad"    || name == "RplSave" || name == "Preload" || name == "OnTransformResetImpl"    || name == "userScript" || name == "constructor" || name == "destructor";
	}

	protected string PropertyTypeName(DataVarType dataType)
	{
		switch (dataType)
		{
			case DataVarType.BOOLEAN:     return "bool";
			case DataVarType.INTEGER:     return "integer";
			case DataVarType.SCALAR:     return "float";
			case DataVarType.VECTOR3:     return "vector";
			case DataVarType.STRING:     return "string";
			case DataVarType.RESOURCE_NAME:     return "resource";
		}
		return string.Empty;
	}

	protected string PropertyOrigin(BaseContainer container, string name)
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

	protected string PropertyRecord(int componentIndex, BaseContainer container, string name, string typeName, string value)
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

	protected bool ReadPropertyRecord(int componentIndex, BaseContainer container, string name, DataVarType dataType, out string record)
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

	protected void AppendPropertyRecords(int componentIndex, BaseContainer definitionSource, BaseContainer valueSource, out string records, out bool truncated)
	{
		array<string> seenNames = new array<string>();
		BaseContainer container = definitionSource;
		int returned;
		for (int depth; container && depth < 16; depth++)
		{
			for (int index, count = container.GetNumVars(); index < count; index++)
			{
				string name = container.GetVarName(index);
				DataVarType dataType = container.GetDataVarType(index);
				string record;
				if (seenNames.Find(name) >= 0 || IsEngineCallback(name))
					continue;
				seenNames.Insert(name);
				if (returned >= 128)
				{
					truncated = true;
					return;
				}
				if (!ReadPropertyRecord(componentIndex, valueSource, name, dataType, record))
				{
					if (!ReadPropertyRecord(componentIndex, container, name, dataType, record))
						continue;
				}
				if (!records.IsEmpty())
					records += ";";
				records += record;
				returned++;
			}
			container = container.GetAncestor();
		}
	}

	protected void ResolveOrigin(RST_WorkbenchInspectEntityResponse response, IEntitySource entity, IEntity runtimeEntity)
	{
		response.resourceReferenceKind = "unresolved";
		if (!runtimeEntity)
			return;
		ResourceManager resourceManager = Workbench.GetModule(ResourceManager);
		if (!resourceManager)
		{
			response.resourceReferenceKind = "resource-manager-unavailable";
			return;
		}
		string entityResourceName = string.Format("%1", entity.GetResourceName());
		string subsceneResourceName = runtimeEntity.GetWorld().GetSubSceneName(entity.GetSubScene());
		MetaFile meta;
		if (!entityResourceName.IsEmpty())
		{
			ResourceName entityResource = entityResourceName;
			meta = resourceManager.GetMetaFile(entityResource.GetPath());
			if (meta)
				response.resourceReferenceKind = "prefab-resource";
		}
		if (!meta && !subsceneResourceName.IsEmpty())
		{
			ResourceName subsceneResource = subsceneResourceName;
			meta = resourceManager.GetMetaFile(subsceneResource.GetPath());
			if (meta)
				response.resourceReferenceKind = "world-subscene";
		}
		if (!meta)
			return;
		response.resourceName = meta.GetResourceID();
		array<string> sourceAddons = new array<string>();
		meta.GetSourceAddons(sourceAddons);
		for (int index = 0; index < sourceAddons.Count(); index++)
		{
			if (index >= 64 || response.contributorAddons.Length() >= 4096)
			{
				response.contributorAddonsTruncated = true;
				break;
			}
			string sourceAddon = sourceAddons[index];
			if (!response.contributorAddons.IsEmpty())
				response.contributorAddons += ";";
			response.contributorAddons += sourceAddon;
		}
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchInspectEntityRequest typedRequest = RST_WorkbenchInspectEntityRequest.Cast(request);
		RST_WorkbenchInspectEntityResponse response = new RST_WorkbenchInspectEntityResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (!worldEditor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = worldEditor.GetApi();
		if (!api)
		{
			response.status = "world-editor-api-unavailable";
			return response;
		}
		response.editorAvailable = true;
		IEntitySource target;
		for (int index = 0, count = api.GetSelectedEntitiesCount(); index < count; index++)
		{
			IEntitySource candidate = api.GetSelectedEntity(index);
			if (candidate && candidate.GetID().ToString() == typedRequest.entityId)
			{
				target = candidate;
				break;
			}
		}
		if (!target)
		{
			for (int index = 0, count = api.GetEditorEntityCount(); index < count; index++)
			{
				IEntitySource candidate = api.GetEditorEntity(index);
				if (candidate && candidate.GetID().ToString() == typedRequest.entityId)
				{
					target = candidate;
					break;
				}
			}
		}
		if (!target)
		{
			response.status = "entity-not-found";
			return response;
		}
		response.status = "available";
		AppendEntity(response.entity, api, target);
		ResolveOrigin(response, target, api.SourceToEntity(target));
		int componentCount = ComponentCount(target);
		for (int index = 0; index < componentCount && index < 64; index++)
		{
			IEntityComponentSource component = ComponentAt(target, index);
			if (!component)
				continue;
			if (!response.components.IsEmpty())
				response.components += ";";
			response.components += string.Format("%1|%2", index, component.GetClassName());
			IEntity runtimeTarget = api.SourceToEntity(target);
			GenericComponent runtimeComponent;
			BaseContainer propertySource = component;
			if (runtimeTarget)
			{
				typename componentType = component.GetClassName().ToType();
				if (componentType)
				{
					runtimeComponent = GenericComponent.Cast(runtimeTarget.FindComponent(componentType));
					if (runtimeComponent)
					{
						BaseContainer runtimeSource = runtimeComponent.GetComponentSource(runtimeTarget);
						if (runtimeSource)
						{
							propertySource = runtimeSource;
						}
					}
				}
			}
			AppendPropertyRecords(index, propertySource, component, response.componentProperties, response.componentPropertiesTruncated);
		}
		BaseContainer parent = target.GetParent();
		for (int index = 0; parent && index < 32; index++)
		{
			IEntitySource parentEntity = IEntitySource.Cast(parent);
			if (parentEntity)
				AppendEntity(response.ancestors, api, parentEntity);
			parent = parent.GetParent();
		}
		response.ancestorsTruncated = parent != null;
		// Stored prefab members can be one-indexed even though GetNumChildren reports
		// their count. Match the prefab inspector's resolved-child indexing here.
		int firstChildIndex;
		if (target.GetNumChildren() > 0 && !target.GetChild(0) && target.GetChild(1))
			firstChildIndex = 1;
		for (int index = 0, count = target.GetNumChildren(), returned = 0; index < count; index++)
		{
			IEntitySource child = IEntitySource.Cast(target.GetChild(index + firstChildIndex));
			if (!child)
				continue;
			if (returned >= 64)
			{
				response.childrenTruncated = true;
				break;
			}
			AppendEntity(response.children, api, child);
			returned++;
		}
		BaseContainer rootDefinition = target;
		ResourceName resourceName = target.GetResourceName();
		if (!resourceName.IsEmpty())
		{
			Resource resource = Resource.Load(resourceName);
			if (resource && resource.IsValid())
			{
				IEntitySource prefabSource = resource.GetResource().ToEntitySource();
				if (prefabSource)
					rootDefinition = prefabSource;
			}
		}
		AppendPropertyRecords(-1, rootDefinition, target, response.properties, response.propertiesTruncated);
		return response;
	}
}
#endif

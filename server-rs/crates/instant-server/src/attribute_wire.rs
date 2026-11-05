//! Borrowed Core attribute encoding. Ordering matches serde_json's sorted maps.
use instant_instaql::{Attribute, AttributeCatalog};
use serde::ser::{Serialize, SerializeMap, Serializer};

pub(crate) struct FrameworkAttribute<'a>(pub(crate) &'a Attribute);

impl Serialize for FrameworkAttribute<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let attribute = self.0;
        let mut map = serializer.serialize_map(Some(15))?;
        // Retain the exact old JSON bytes, not merely equivalent parsed objects:
        // the realtime catalog fingerprint is over the wire representation.
        map.serialize_entry("cardinality", &attribute.cardinality)?;
        map.serialize_entry(
            "catalog",
            if attribute.etype.starts_with('$') {
                "system"
            } else {
                "user"
            },
        )?;
        map.serialize_entry("checked-data-type", &attribute.checked_data_type)?;
        map.serialize_entry(
            "forward-identity",
            &(
                attribute.forward_identity_id(),
                &attribute.etype,
                &attribute.label,
            ),
        )?;
        map.serialize_entry("id", &attribute.id)?;
        map.serialize_entry("index?", &attribute.indexed)?;
        map.serialize_entry("indexing?", &attribute.indexing)?;
        map.serialize_entry("inferred-types", &attribute.inferred_types())?;
        map.serialize_entry("on-delete", &attribute.on_delete)?;
        map.serialize_entry("on-delete-reverse", &attribute.on_delete_reverse)?;
        map.serialize_entry("required?", &attribute.required)?;
        let reverse = attribute
            .reverse_etype
            .as_ref()
            .zip(attribute.reverse_label.as_ref())
            .map(|(etype, label)| (attribute.reverse_identity_id(), etype, label));
        map.serialize_entry("reverse-identity", &reverse)?;
        map.serialize_entry("setting-unique?", &attribute.setting_unique)?;
        map.serialize_entry("unique?", &attribute.unique)?;
        map.serialize_entry("value-type", &attribute.value_type)?;
        map.end()
    }
}

pub(crate) struct FrameworkCatalog<'a>(pub(crate) &'a AttributeCatalog);

impl Serialize for FrameworkCatalog<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.attributes.iter().map(FrameworkAttribute))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_instaql::{AttributeValueType, AttributeWireMetadata, Cardinality, DeleteAction};
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn legacy(attribute: &Attribute) -> Value {
        json!({
            "id": attribute.id,
            "value-type": match attribute.value_type { AttributeValueType::Blob => "blob", AttributeValueType::Ref => "ref" },
            "cardinality": match attribute.cardinality { Cardinality::One => "one", Cardinality::Many => "many" },
            "forward-identity": [attribute.forward_identity_id(), &attribute.etype, &attribute.label],
            "reverse-identity": attribute.reverse_etype.as_ref().zip(attribute.reverse_label.as_ref())
                .map(|(etype, label)| json!([attribute.reverse_identity_id(), etype, label])),
            "unique?": attribute.unique, "index?": attribute.indexed, "required?": attribute.required,
            "inferred-types": attribute.inferred_types(),
            "catalog": if attribute.etype.starts_with('$') { "system" } else { "user" },
            "on-delete": attribute.on_delete, "on-delete-reverse": attribute.on_delete_reverse,
            "checked-data-type": attribute.checked_data_type, "indexing?": attribute.indexing,
            "setting-unique?": attribute.setting_unique,
        })
    }

    fn fixture() -> Attribute {
        Attribute {
            id: Uuid::from_u128(1),
            wire: None,
            etype: "items".into(),
            label: "é\"\\\n🦀".into(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    #[test]
    fn borrowed_attributes_preserve_exact_legacy_bytes_for_all_wire_fields() {
        let mut fixtures = vec![fixture()];
        for flags in 0..64 {
            let mut attribute = fixture();
            attribute.id = Uuid::from_u128(100 + flags);
            attribute.etype = "$system\u{0}".into();
            attribute.value_type = AttributeValueType::Ref;
            attribute.cardinality = Cardinality::Many;
            attribute.unique = flags & 1 != 0;
            attribute.setting_unique = flags & 2 != 0;
            attribute.indexed = flags & 4 != 0;
            attribute.required = flags & 8 != 0;
            attribute.indexing = flags & 16 != 0;
            attribute.checking_data_type = flags & 32 != 0;
            attribute.reverse_etype = Some("reverse\"".into());
            attribute.reverse_label = Some("field\\".into());
            attribute.wire = Some(AttributeWireMetadata {
                forward_identity_id: Uuid::from_u128(2),
                reverse_identity_id: Some(Uuid::from_u128(3)),
                inferred_types: Some(vec!["string".into(), "number".into()]),
            });
            attribute.on_delete = Some(DeleteAction::Cascade);
            attribute.on_delete_reverse = Some(DeleteAction::Cascade);
            attribute.checked_data_type = Some(instant_datalog::DataType::String);
            fixtures.push(attribute);
        }
        let mut partial = fixtures.last().unwrap().clone();
        partial.reverse_label = None;
        partial.wire.as_mut().unwrap().inferred_types = Some(vec![]);
        fixtures.push(partial);
        let mut fallback = fixtures[1].clone();
        fallback.wire.as_mut().unwrap().reverse_identity_id = None;
        fixtures.push(fallback);
        for data_type in [
            instant_datalog::DataType::String,
            instant_datalog::DataType::Number,
            instant_datalog::DataType::Boolean,
            instant_datalog::DataType::Date,
        ] {
            let mut checked = fixture();
            checked.checked_data_type = Some(data_type);
            fixtures.push(checked);
        }
        for attribute in &fixtures {
            assert_eq!(
                serde_json::to_vec(&FrameworkAttribute(attribute)).unwrap(),
                serde_json::to_vec(&legacy(attribute)).unwrap()
            );
        }
        let catalog = AttributeCatalog {
            attributes: fixtures,
        };
        assert_eq!(
            serde_json::to_vec(&FrameworkCatalog(&catalog)).unwrap(),
            serde_json::to_vec(&catalog.attributes.iter().map(legacy).collect::<Vec<_>>()).unwrap()
        );
    }

    #[test]
    fn fresh_catalog_mutations_change_the_borrowed_wire_without_cached_values() {
        let mut catalog = AttributeCatalog {
            attributes: vec![fixture()],
        };
        let before = serde_json::to_vec(&FrameworkCatalog(&catalog)).unwrap();
        catalog.attributes[0].label.push_str("changed");
        assert_ne!(
            before,
            serde_json::to_vec(&FrameworkCatalog(&catalog)).unwrap()
        );
        catalog.attributes.clear();
        assert_eq!(
            serde_json::to_vec(&FrameworkCatalog(&catalog)).unwrap(),
            b"[]"
        );
    }

    #[test]
    #[ignore = "manual catalog allocation/serialization benchmark; not a timing gate"]
    fn borrowed_catalog_benchmark() {
        use sha2::{Digest, Sha256};
        use std::hint::black_box;
        use std::time::Instant;
        struct Writer(Sha256);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let catalog = AttributeCatalog {
            attributes: (0..80).map(|_| fixture()).collect(),
        };
        for borrowed in [false, true] {
            let started = Instant::now();
            for _ in 0..2000 {
                let mut writer = Writer(Sha256::new());
                if borrowed {
                    serde_json::to_writer(&mut writer, &FrameworkCatalog(black_box(&catalog)))
                        .unwrap();
                } else {
                    let attrs = Value::Array(catalog.attributes.iter().map(legacy).collect());
                    serde_json::to_writer(&mut writer, &attrs).unwrap();
                }
                black_box(writer.0.finalize());
            }
            println!(
                "catalog-wire borrowed={borrowed} iterations=2000 attrs=80 elapsed_us={}",
                started.elapsed().as_micros()
            );
        }
    }
}

// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Defines concat kernel for `ArrayRef`
//!
//! Example:
//!
//! ```
//! use arrow::array::{ArrayRef, StringArray};
//! use arrow::compute::concat;
//!
//! let arr = concat(&[
//!     &StringArray::from(vec!["hello", "world"]),
//!     &StringArray::from(vec!["!"]),
//! ]).unwrap();
//! assert_eq!(arr.len(), 3);
//! ```

use crate::array::*;
use crate::datatypes::DataType;
use crate::error::{ArrowError, Result};

fn compute_str_values_length<Offset: OffsetSizeTrait>(arrays: &[&ArrayData]) -> usize {
    arrays
        .iter()
        .map(|&data| {
            // get the length of the value buffer
            let buf_len = data.buffers()[1].len();
            // find the offset of the buffer
            // this returns a slice of offsets, starting from the offset of the array
            // so we can take the first value
            let offset = data.buffer::<Offset>(0)[0];
            buf_len - offset.to_usize().unwrap()
        })
        .sum()
}

/// Concatenate multiple [Array] of the same type into a single [ArrayRef].
pub fn concat(arrays: &[&dyn Array]) -> Result<ArrayRef> {
    if arrays.is_empty() {
        return Err(ArrowError::ComputeError(
            "concat requires input of at least one array".to_string(),
        ));
    } else if arrays.len() == 1 {
        let array = arrays[0];
        return Ok(array.slice(0, array.len()));
    }

    if arrays
        .iter()
        .any(|array| array.data_type() != arrays[0].data_type())
    {
        return Err(ArrowError::InvalidArgumentError(
            "It is not possible to concatenate arrays of different data types."
                .to_string(),
        ));
    }

    let lengths = arrays.iter().map(|array| array.len()).collect::<Vec<_>>();
    let capacity = lengths.iter().sum();

    let arrays = arrays.iter().map(|a| a.data()).collect::<Vec<_>>();

    let mut mutable = match arrays[0].data_type() {
        DataType::Utf8 => {
            let str_values_size = compute_str_values_length::<i32>(&arrays);
            MutableArrayData::with_capacities(
                arrays,
                false,
                Capacities::Binary(capacity, Some(str_values_size)),
            )
        }
        DataType::LargeUtf8 => {
            let str_values_size = compute_str_values_length::<i64>(&arrays);
            MutableArrayData::with_capacities(
                arrays,
                false,
                Capacities::Binary(capacity, Some(str_values_size)),
            )
        }
        _ => MutableArrayData::new(arrays, false, capacity),
    };

    for (i, len) in lengths.iter().enumerate() {
        mutable.extend(i, 0, *len)
    }

    Ok(make_array(mutable.freeze()))
}

// Elementwise concatenation of StringArrays
pub fn string_concat<Offset: OffsetSizeTrait>(
    left: &GenericStringArray<Offset>,
    right: &GenericStringArray<Offset>,
) -> Result<GenericStringArray<Offset>> {
    // TODO: Handle non-zero offset in source ArrayData

    if left.len() != right.len() {
        return Err(ArrowError::ComputeError(
            "StringArrays must have the same length".to_string(),
        ));
    }

    let output_bitmap = match (left.data().null_bitmap(), right.data().null_bitmap()) {
        (Some(left_bitmap), Some(right_bitmap)) => Some((left_bitmap & right_bitmap)?),
        (Some(left_bitmap), None) => Some(left_bitmap.clone()),
        (None, Some(right_bitmap)) => Some(right_bitmap.clone()),
        (None, None) => None,
    };

    let left_offsets = left.value_offsets();
    let right_offsets = right.value_offsets();

    let left_buffer = left.value_data();
    let right_buffer = right.value_data();
    let left_values = left_buffer.as_slice();
    let right_values = right_buffer.as_slice();

    let mut output_offsets = BufferBuilder::<Offset>::new(left_offsets.len());
    let mut output_values =
        BufferBuilder::<u8>::new(left_values.len() + right_values.len());

    output_offsets.append(Offset::zero());
    for (idx, (l, r)) in left_offsets
        .windows(2)
        .zip(right_offsets.windows(2))
        .enumerate()
    {
        match &output_bitmap {
            Some(bitmap) if { bitmap.is_set(idx) } => {
                output_values.append_slice(
                    &left_values[l[0].to_usize().unwrap()..l[1].to_usize().unwrap()],
                );
                output_values.append_slice(
                    &right_values[r[0].to_usize().unwrap()..r[1].to_usize().unwrap()],
                )
            }
            _ => (),
        }
        output_offsets.append(Offset::from_usize(output_values.len()).unwrap());
    }

    let mut builder =
        ArrayDataBuilder::new(GenericStringArray::<Offset>::get_data_type())
            .len(left.len())
            .add_buffer(output_offsets.finish())
            .add_buffer(output_values.finish());

    if let Some(output_bitmap) = output_bitmap {
        builder = builder.null_bit_buffer(output_bitmap.into_buffer());
    }

    // SAFETY - offsets valid by construction
    Ok(unsafe { builder.build_unchecked() }.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::*;
    use std::sync::Arc;

    #[test]
    fn test_concat_empty_vec() {
        let re = concat(&[]);
        assert!(re.is_err());
    }

    #[test]
    fn test_concat_one_element_vec() -> Result<()> {
        let arr = Arc::new(PrimitiveArray::<Int64Type>::from(vec![
            Some(-1),
            Some(2),
            None,
        ])) as ArrayRef;
        let result = concat(&[arr.as_ref()])?;
        assert_eq!(
            &arr, &result,
            "concatenating single element array gives back the same result"
        );
        Ok(())
    }

    #[test]
    fn test_concat_incompatible_datatypes() {
        let re = concat(&[
            &PrimitiveArray::<Int64Type>::from(vec![Some(-1), Some(2), None]),
            &StringArray::from(vec![Some("hello"), Some("bar"), Some("world")]),
        ]);
        assert!(re.is_err());
    }

    #[test]
    fn test_concat_string_arrays() -> Result<()> {
        let arr = concat(&[
            &StringArray::from(vec!["hello", "world"]),
            &StringArray::from(vec!["2", "3", "4"]),
            &StringArray::from(vec![Some("foo"), Some("bar"), None, Some("baz")]),
        ])?;

        let expected_output = Arc::new(StringArray::from(vec![
            Some("hello"),
            Some("world"),
            Some("2"),
            Some("3"),
            Some("4"),
            Some("foo"),
            Some("bar"),
            None,
            Some("baz"),
        ])) as ArrayRef;

        assert_eq!(&arr, &expected_output);

        Ok(())
    }

    #[test]
    fn test_concat_primitive_arrays() -> Result<()> {
        let arr = concat(&[
            &PrimitiveArray::<Int64Type>::from(vec![
                Some(-1),
                Some(-1),
                Some(2),
                None,
                None,
            ]),
            &PrimitiveArray::<Int64Type>::from(vec![
                Some(101),
                Some(102),
                Some(103),
                None,
            ]),
            &PrimitiveArray::<Int64Type>::from(vec![Some(256), Some(512), Some(1024)]),
        ])?;

        let expected_output = Arc::new(PrimitiveArray::<Int64Type>::from(vec![
            Some(-1),
            Some(-1),
            Some(2),
            None,
            None,
            Some(101),
            Some(102),
            Some(103),
            None,
            Some(256),
            Some(512),
            Some(1024),
        ])) as ArrayRef;

        assert_eq!(&arr, &expected_output);

        Ok(())
    }

    #[test]
    fn test_concat_primitive_array_slices() -> Result<()> {
        let input_1 = PrimitiveArray::<Int64Type>::from(vec![
            Some(-1),
            Some(-1),
            Some(2),
            None,
            None,
        ])
        .slice(1, 3);

        let input_2 = PrimitiveArray::<Int64Type>::from(vec![
            Some(101),
            Some(102),
            Some(103),
            None,
        ])
        .slice(1, 3);
        let arr = concat(&[input_1.as_ref(), input_2.as_ref()])?;

        let expected_output = Arc::new(PrimitiveArray::<Int64Type>::from(vec![
            Some(-1),
            Some(2),
            None,
            Some(102),
            Some(103),
            None,
        ])) as ArrayRef;

        assert_eq!(&arr, &expected_output);

        Ok(())
    }

    #[test]
    fn test_concat_boolean_primitive_arrays() -> Result<()> {
        let arr = concat(&[
            &BooleanArray::from(vec![
                Some(true),
                Some(true),
                Some(false),
                None,
                None,
                Some(false),
            ]),
            &BooleanArray::from(vec![None, Some(false), Some(true), Some(false)]),
        ])?;

        let expected_output = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(true),
            Some(false),
            None,
            None,
            Some(false),
            None,
            Some(false),
            Some(true),
            Some(false),
        ])) as ArrayRef;

        assert_eq!(&arr, &expected_output);

        Ok(())
    }

    #[test]
    fn test_concat_primitive_list_arrays() -> Result<()> {
        let list1 = vec![
            Some(vec![Some(-1), Some(-1), Some(2), None, None]),
            Some(vec![]),
            None,
            Some(vec![Some(10)]),
        ];
        let list1_array =
            ListArray::from_iter_primitive::<Int64Type, _, _>(list1.clone());

        let list2 = vec![
            None,
            Some(vec![Some(100), None, Some(101)]),
            Some(vec![Some(102)]),
        ];
        let list2_array =
            ListArray::from_iter_primitive::<Int64Type, _, _>(list2.clone());

        let list3 = vec![Some(vec![Some(1000), Some(1001)])];
        let list3_array =
            ListArray::from_iter_primitive::<Int64Type, _, _>(list3.clone());

        let array_result = concat(&[&list1_array, &list2_array, &list3_array])?;

        let expected = list1
            .into_iter()
            .chain(list2.into_iter())
            .chain(list3.into_iter());
        let array_expected = ListArray::from_iter_primitive::<Int64Type, _, _>(expected);

        assert_eq!(array_result.as_ref(), &array_expected as &dyn Array);

        Ok(())
    }

    #[test]
    fn test_concat_struct_arrays() -> Result<()> {
        let field = Field::new("field", DataType::Int64, true);
        let input_primitive_1: ArrayRef =
            Arc::new(PrimitiveArray::<Int64Type>::from(vec![
                Some(-1),
                Some(-1),
                Some(2),
                None,
                None,
            ]));
        let input_struct_1 = StructArray::from(vec![(field.clone(), input_primitive_1)]);

        let input_primitive_2: ArrayRef =
            Arc::new(PrimitiveArray::<Int64Type>::from(vec![
                Some(101),
                Some(102),
                Some(103),
                None,
            ]));
        let input_struct_2 = StructArray::from(vec![(field.clone(), input_primitive_2)]);

        let input_primitive_3: ArrayRef =
            Arc::new(PrimitiveArray::<Int64Type>::from(vec![
                Some(256),
                Some(512),
                Some(1024),
            ]));
        let input_struct_3 = StructArray::from(vec![(field, input_primitive_3)]);

        let arr = concat(&[&input_struct_1, &input_struct_2, &input_struct_3])?;

        let expected_primitive_output = Arc::new(PrimitiveArray::<Int64Type>::from(vec![
            Some(-1),
            Some(-1),
            Some(2),
            None,
            None,
            Some(101),
            Some(102),
            Some(103),
            None,
            Some(256),
            Some(512),
            Some(1024),
        ])) as ArrayRef;

        let actual_primitive = arr
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap()
            .column(0);
        assert_eq!(actual_primitive, &expected_primitive_output);

        Ok(())
    }

    #[test]
    fn test_concat_struct_array_slices() -> Result<()> {
        let field = Field::new("field", DataType::Int64, true);
        let input_primitive_1: ArrayRef =
            Arc::new(PrimitiveArray::<Int64Type>::from(vec![
                Some(-1),
                Some(-1),
                Some(2),
                None,
                None,
            ]));
        let input_struct_1 = StructArray::from(vec![(field.clone(), input_primitive_1)]);

        let input_primitive_2: ArrayRef =
            Arc::new(PrimitiveArray::<Int64Type>::from(vec![
                Some(101),
                Some(102),
                Some(103),
                None,
            ]));
        let input_struct_2 = StructArray::from(vec![(field, input_primitive_2)]);

        let arr = concat(&[
            input_struct_1.slice(1, 3).as_ref(),
            input_struct_2.slice(1, 2).as_ref(),
        ])?;

        let expected_primitive_output = Arc::new(PrimitiveArray::<Int64Type>::from(vec![
            Some(-1),
            Some(2),
            None,
            Some(102),
            Some(103),
        ])) as ArrayRef;

        let actual_primitive = arr
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap()
            .column(0);
        assert_eq!(actual_primitive, &expected_primitive_output);

        Ok(())
    }

    #[test]
    fn test_string_array_slices() -> Result<()> {
        let input_1 = StringArray::from(vec!["hello", "A", "B", "C"]);
        let input_2 = StringArray::from(vec!["world", "D", "E", "Z"]);

        let arr = concat(&[input_1.slice(1, 3).as_ref(), input_2.slice(1, 2).as_ref()])?;

        let expected_output = StringArray::from(vec!["A", "B", "C", "D", "E"]);

        let actual_output = arr.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(actual_output, &expected_output);

        Ok(())
    }

    #[test]
    fn test_string_array_with_null_slices() -> Result<()> {
        let input_1 = StringArray::from(vec![Some("hello"), None, Some("A"), Some("C")]);
        let input_2 = StringArray::from(vec![None, Some("world"), Some("D"), None]);

        let arr = concat(&[input_1.slice(1, 3).as_ref(), input_2.slice(1, 2).as_ref()])?;

        let expected_output =
            StringArray::from(vec![None, Some("A"), Some("C"), Some("world"), Some("D")]);

        let actual_output = arr.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(actual_output, &expected_output);

        Ok(())
    }

    fn collect_string_dictionary(
        dictionary: &DictionaryArray<Int32Type>,
    ) -> Vec<Option<String>> {
        let values = dictionary.values();
        let values = values.as_any().downcast_ref::<StringArray>().unwrap();

        dictionary
            .keys()
            .iter()
            .map(|key| key.map(|key| values.value(key as _).to_string()))
            .collect()
    }

    fn concat_dictionary(
        input_1: DictionaryArray<Int32Type>,
        input_2: DictionaryArray<Int32Type>,
    ) -> Vec<Option<String>> {
        let concat = concat(&[&input_1 as _, &input_2 as _]).unwrap();
        let concat = concat
            .as_any()
            .downcast_ref::<DictionaryArray<Int32Type>>()
            .unwrap();

        collect_string_dictionary(concat)
    }

    #[test]
    fn test_string_dictionary_array() {
        let input_1: DictionaryArray<Int32Type> =
            vec!["hello", "A", "B", "hello", "hello", "C"]
                .into_iter()
                .collect();
        let input_2: DictionaryArray<Int32Type> =
            vec!["hello", "E", "E", "hello", "F", "E"]
                .into_iter()
                .collect();

        let expected: Vec<_> = vec![
            "hello", "A", "B", "hello", "hello", "C", "hello", "E", "E", "hello", "F",
            "E",
        ]
        .into_iter()
        .map(|x| Some(x.to_string()))
        .collect();

        let concat = concat_dictionary(input_1, input_2);
        assert_eq!(concat, expected);
    }

    #[test]
    fn test_string_dictionary_array_nulls() {
        let input_1: DictionaryArray<Int32Type> =
            vec![Some("foo"), Some("bar"), None, Some("fiz")]
                .into_iter()
                .collect();
        let input_2: DictionaryArray<Int32Type> = vec![None].into_iter().collect();
        let expected = vec![
            Some("foo".to_string()),
            Some("bar".to_string()),
            None,
            Some("fiz".to_string()),
            None,
        ];

        let concat = concat_dictionary(input_1, input_2);
        assert_eq!(concat, expected);
    }

    #[test]
    fn test_concat_string_sizes() -> Result<()> {
        let a: LargeStringArray = ((0..150).map(|_| Some("foo"))).collect();
        let b: LargeStringArray = ((0..150).map(|_| Some("foo"))).collect();
        let c = LargeStringArray::from(vec![Some("foo"), Some("bar"), None, Some("baz")]);
        // 150 * 3 = 450
        // 150 * 3 = 450
        // 3 * 3   = 9
        // ------------+
        // 909
        // closest 64 byte aligned cap = 960

        let arr = concat(&[&a, &b, &c])?;
        // this would have been 1280 if we did not precompute the value lengths.
        assert_eq!(arr.data().buffers()[1].capacity(), 960);

        Ok(())
    }

    #[test]
    fn test_dictionary_concat_reuse() {
        let array: DictionaryArray<Int8Type> =
            vec!["a", "a", "b", "c"].into_iter().collect();
        let copy: DictionaryArray<Int8Type> = array.data().clone().into();

        // dictionary is "a", "b", "c"
        assert_eq!(
            array.values(),
            &(Arc::new(StringArray::from(vec!["a", "b", "c"])) as ArrayRef)
        );
        assert_eq!(array.keys(), &Int8Array::from(vec![0, 0, 1, 2]));

        // concatenate it with itself
        let combined = concat(&[&copy as _, &array as _]).unwrap();

        let combined = combined
            .as_any()
            .downcast_ref::<DictionaryArray<Int8Type>>()
            .unwrap();

        assert_eq!(
            combined.values(),
            &(Arc::new(StringArray::from(vec!["a", "b", "c"])) as ArrayRef),
            "Actual: {:#?}",
            combined
        );

        assert_eq!(
            combined.keys(),
            &Int8Array::from(vec![0, 0, 1, 2, 0, 0, 1, 2])
        );

        // Should have reused the dictionary
        assert!(array.data().child_data()[0].ptr_eq(&combined.data().child_data()[0]));
        assert!(copy.data().child_data()[0].ptr_eq(&combined.data().child_data()[0]));

        let new: DictionaryArray<Int8Type> = vec!["d"].into_iter().collect();
        let combined = concat(&[&copy as _, &array as _, &new as _]).unwrap();

        // Should not have reused the dictionary
        assert!(!array.data().child_data()[0].ptr_eq(&combined.data().child_data()[0]));
        assert!(!copy.data().child_data()[0].ptr_eq(&combined.data().child_data()[0]));
        assert!(!new.data().child_data()[0].ptr_eq(&combined.data().child_data()[0]));
    }

    #[cfg(feature = "test_utils")]
    #[test]
    fn test_string_concat() {
        let left = [Some("foo"), Some("bar"), None]
            .into_iter()
            .collect::<StringArray>();
        let right = [None, Some("yyy"), Some("zzz")]
            .into_iter()
            .collect::<StringArray>();

        let res = string_concat(&left, &right).unwrap();

        let expected = [None, Some("baryyy"), None]
            .into_iter()
            .collect::<StringArray>();

        assert_eq!(res, expected);
    }
}
